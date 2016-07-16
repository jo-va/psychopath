#![allow(dead_code)]

use std;
use std::cmp::Ordering;
use quickersort::sort_by;
use lerp::lerp_slice;
use bbox::BBox;
use boundable::Boundable;
use ray::AccelRay;
use algorithm::{partition, merge_slices_append};
use math::log2_64;

const BVH_MAX_DEPTH: usize = 64;
const SAH_BIN_COUNT: usize = 13; // Prime numbers work best, for some reason

#[derive(Debug)]
pub struct BVH {
    nodes: Vec<BVHNode>,
    bounds: Vec<BBox>,
    depth: usize,
    bounds_cache: Vec<BBox>,
}

#[derive(Debug)]
enum BVHNode {
    Internal {
        bounds_range: (usize, usize),
        second_child_index: usize,
        split_axis: u8,
    },

    Leaf {
        bounds_range: (usize, usize),
        object_range: (usize, usize),
    },
}

impl BVH {
    pub fn new_empty() -> BVH {
        BVH {
            nodes: Vec::new(),
            bounds: Vec::new(),
            depth: 0,
            bounds_cache: Vec::new(),
        }
    }

    pub fn from_objects<'a, T, F>(objects: &mut [T], objects_per_leaf: usize, bounder: F) -> BVH
        where F: 'a + Fn(&T) -> &'a [BBox]
    {
        let mut bvh = BVH::new_empty();

        bvh.recursive_build(0, 0, objects_per_leaf, objects, &bounder);
        bvh.bounds_cache.clear();
        bvh.bounds_cache.shrink_to_fit();

        bvh
    }

    pub fn tree_depth(&self) -> usize {
        self.depth
    }

    fn acc_bounds<'a, T, F>(&mut self, objects1: &mut [T], bounder: &F)
        where F: 'a + Fn(&T) -> &'a [BBox]
    {
        // TODO: merging of different length bounds
        self.bounds_cache.clear();
        for bb in bounder(&objects1[0]).iter() {
            self.bounds_cache.push(*bb);
        }
        for obj in &objects1[1..] {
            let bounds = bounder(obj);
            debug_assert!(self.bounds_cache.len() == bounds.len());
            for i in 0..bounds.len() {
                self.bounds_cache[i] = self.bounds_cache[i] | bounds[i];
            }
        }
    }

    fn recursive_build<'a, T, F>(&mut self,
                                 offset: usize,
                                 depth: usize,
                                 objects_per_leaf: usize,
                                 objects: &mut [T],
                                 bounder: &F)
                                 -> (usize, (usize, usize))
        where F: 'a + Fn(&T) -> &'a [BBox]
    {
        let me = self.nodes.len();

        if objects.len() == 0 {
            return (0, (0, 0));
        } else if objects.len() <= objects_per_leaf {
            // Leaf node
            self.acc_bounds(objects, bounder);
            let bi = self.bounds.len();
            for b in self.bounds_cache.iter() {
                self.bounds.push(*b);
            }
            self.nodes.push(BVHNode::Leaf {
                bounds_range: (bi, self.bounds.len()),
                object_range: (offset, offset + objects.len()),
            });

            if self.depth < depth {
                self.depth = depth;
            }

            return (me, (bi, self.bounds.len()));
        } else {
            // Not a leaf node
            self.nodes.push(BVHNode::Internal {
                bounds_range: (0, 0),
                second_child_index: 0,
                split_axis: 0,
            });

            // Get combined object bounds
            let bounds = {
                let mut bb = BBox::new();
                for obj in &objects[..] {
                    bb |= lerp_slice(bounder(obj), 0.5);
                }
                bb
            };

            // Partition objects.
            // If we're too near the max depth, we do balanced building to
            // avoid exceeding max depth.
            // Otherwise we do SAH splitting to build better trees.
            let (split_index, split_axis) = if (log2_64(objects.len() as u64) as usize) <
                                               (BVH_MAX_DEPTH - depth) {
                // SAH splitting, when we have room to play

                // Pre-calc SAH div points
                let sah_divs = {
                    let mut sah_divs = [[0.0f32; SAH_BIN_COUNT - 1]; 3];
                    for d in 0..3 {
                        let extent = bounds.max[d] - bounds.min[d];
                        for div in 0..(SAH_BIN_COUNT - 1) {
                            let part = extent * ((div + 1) as f32 / SAH_BIN_COUNT as f32);
                            sah_divs[d][div] = bounds.min[d] + part;
                        }
                    }
                    sah_divs
                };

                // Build SAH bins
                let sah_bins = {
                    let mut sah_bins = [[(BBox::new(), BBox::new(), 0, 0); SAH_BIN_COUNT - 1]; 3];
                    for obj in objects.iter() {
                        let tb = lerp_slice(bounder(obj), 0.5);
                        let centroid = (tb.min.into_vector() + tb.max.into_vector()) * 0.5;

                        for d in 0..3 {
                            for div in 0..(SAH_BIN_COUNT - 1) {
                                if centroid[d] <= sah_divs[d][div] {
                                    sah_bins[d][div].0 |= tb;
                                    sah_bins[d][div].2 += 1;
                                } else {
                                    sah_bins[d][div].1 |= tb;
                                    sah_bins[d][div].3 += 1;
                                }
                            }
                        }
                    }
                    sah_bins
                };

                // Find best split axis and div point
                let (split_axis, div) = {
                    let mut dim = 0;
                    let mut div_n = 0.0;
                    let mut smallest_cost = std::f32::INFINITY;

                    for d in 0..3 {
                        for div in 0..(SAH_BIN_COUNT - 1) {
                            let left_cost = sah_bins[d][div].0.surface_area() *
                                            sah_bins[d][div].2 as f32;
                            let right_cost = sah_bins[d][div].1.surface_area() *
                                             sah_bins[d][div].3 as f32;
                            let tot_cost = left_cost + right_cost;
                            if tot_cost < smallest_cost {
                                dim = d;
                                div_n = sah_divs[d][div];
                                smallest_cost = tot_cost;
                            }
                        }
                    }

                    (dim, div_n)
                };

                // Partition
                let mut split_i = partition(&mut objects[..], |obj| {
                    let tb = lerp_slice(bounder(obj), 0.5);
                    let centroid = (tb.min[split_axis] + tb.max[split_axis]) * 0.5;
                    centroid < div
                });
                if split_i < 1 {
                    split_i = 1;
                } else if split_i >= objects.len() {
                    split_i = objects.len() - 1;
                }

                (split_i, split_axis)
            } else {
                // Balanced splitting, when we don't have room to play
                let split_axis = {
                    let mut axis = 0;
                    let mut largest = std::f32::NEG_INFINITY;
                    for i in 0..3 {
                        let extent = bounds.max[i] - bounds.min[i];
                        if extent > largest {
                            largest = extent;
                            axis = i;
                        }
                    }
                    axis
                };

                sort_by(objects,
                        &|a, b| {
                    let tb_a = lerp_slice(bounder(a), 0.5);
                    let tb_b = lerp_slice(bounder(b), 0.5);
                    let centroid_a = (tb_a.min[split_axis] + tb_a.max[split_axis]) * 0.5;
                    let centroid_b = (tb_b.min[split_axis] + tb_b.max[split_axis]) * 0.5;

                    if centroid_a < centroid_b {
                        Ordering::Less
                    } else if centroid_a == centroid_b {
                        Ordering::Equal
                    } else {
                        Ordering::Greater
                    }
                });

                (objects.len() / 2, split_axis)
            };

            // Create child nodes
            let (_, c1_bounds) = self.recursive_build(offset,
                                                      depth + 1,
                                                      objects_per_leaf,
                                                      &mut objects[..split_index],
                                                      bounder);
            let (c2_index, c2_bounds) = self.recursive_build(offset + split_index,
                                                             depth + 1,
                                                             objects_per_leaf,
                                                             &mut objects[split_index..],
                                                             bounder);

            // Determine bounds
            // TODO: do merging without the temporary vec.
            let bi = self.bounds.len();
            let mut merged = Vec::new();
            merge_slices_append(&self.bounds[c1_bounds.0..c1_bounds.1],
                                &self.bounds[c2_bounds.0..c2_bounds.1],
                                &mut merged,
                                |b1, b2| *b1 | *b2);
            self.bounds.extend(merged.drain(0..));

            // Set node
            self.nodes[me] = BVHNode::Internal {
                bounds_range: (bi, self.bounds.len()),
                second_child_index: c2_index,
                split_axis: split_axis as u8,
            };

            return (me, (bi, self.bounds.len()));
        }
    }


    pub fn traverse<T, F>(&self, rays: &mut [AccelRay], objects: &[T], mut obj_ray_test: F)
        where F: FnMut(&T, &mut [AccelRay])
    {
        if self.nodes.len() == 0 {
            return;
        }

        // +2 of max depth for root and last child
        let mut i_stack = [0; BVH_MAX_DEPTH + 2];
        let mut ray_i_stack = [rays.len(); BVH_MAX_DEPTH + 2];
        let mut stack_ptr = 1;

        while stack_ptr > 0 {
            match self.nodes[i_stack[stack_ptr]] {
                BVHNode::Internal { bounds_range: br, second_child_index, split_axis } => {
                    let part = partition(&mut rays[..ray_i_stack[stack_ptr]], |r| {
                        (!r.is_done()) &&
                        lerp_slice(&self.bounds[br.0..br.1], r.time).intersect_accel_ray(r)
                    });
                    if part > 0 {
                        i_stack[stack_ptr] += 1;
                        i_stack[stack_ptr + 1] = second_child_index;
                        ray_i_stack[stack_ptr] = part;
                        ray_i_stack[stack_ptr + 1] = part;
                        if rays[0].dir_inv[split_axis as usize].is_sign_positive() {
                            i_stack.swap(stack_ptr, stack_ptr + 1);
                        }
                        stack_ptr += 1;
                    } else {
                        stack_ptr -= 1;
                    }
                }

                BVHNode::Leaf { bounds_range: br, object_range } => {
                    let part = partition(&mut rays[..ray_i_stack[stack_ptr]], |r| {
                        (!r.is_done()) &&
                        lerp_slice(&self.bounds[br.0..br.1], r.time).intersect_accel_ray(r)
                    });
                    if part > 0 {
                        for obj in &objects[object_range.0..object_range.1] {
                            obj_ray_test(obj, &mut rays[..part]);
                        }
                    }

                    stack_ptr -= 1;
                }
            }
        }
    }
}


impl Boundable for BVH {
    fn bounds<'a>(&'a self) -> &'a [BBox] {
        match self.nodes[0] {
            BVHNode::Internal { bounds_range, .. } => &self.bounds[bounds_range.0..bounds_range.1],

            BVHNode::Leaf { bounds_range, .. } => &self.bounds[bounds_range.0..bounds_range.1],
        }
    }
}
