use std::iter;

use algorithm::partition;
use lerp::lerp_slice;
use ray::{Ray, AccelRay};
use scene::{Assembly, Object, InstanceType};
use surface::SurfaceIntersection;
use transform_stack::TransformStack;
use shading::{SurfaceShader, SimpleSurfaceShader};
use color::{XYZ, rec709_to_xyz};


pub struct Tracer<'a> {
    rays: Vec<AccelRay>,
    inner: TracerInner<'a>,
}

impl<'a> Tracer<'a> {
    pub fn from_assembly(assembly: &'a Assembly) -> Tracer<'a> {
        Tracer {
            rays: Vec::new(),
            inner: TracerInner {
                root: assembly,
                xform_stack: TransformStack::new(),
                isects: Vec::new(),
            },
        }
    }

    pub fn trace<'b>(&'b mut self, wrays: &[Ray]) -> &'b [SurfaceIntersection] {
        self.rays.clear();
        self.rays.reserve(wrays.len());
        let mut ids = 0..(wrays.len() as u32);
        self.rays.extend(wrays.iter().map(
            |wr| AccelRay::new(wr, ids.next().unwrap()),
        ));

        self.inner.trace(wrays, &mut self.rays[..])
    }
}

struct TracerInner<'a> {
    root: &'a Assembly<'a>,
    xform_stack: TransformStack,
    isects: Vec<SurfaceIntersection>,
}

impl<'a> TracerInner<'a> {
    fn trace<'b>(&'b mut self, wrays: &[Ray], rays: &mut [AccelRay]) -> &'b [SurfaceIntersection] {
        // Ready the isects
        self.isects.clear();
        self.isects.reserve(wrays.len());
        self.isects.extend(
            iter::repeat(SurfaceIntersection::Miss).take(
                wrays
                    .len(),
            ),
        );

        let mut ray_sets = split_rays_by_direction(&mut rays[..]);
        for ray_set in ray_sets.iter_mut().filter(|ray_set| !ray_set.is_empty()) {
            self.trace_assembly(self.root, wrays, ray_set);
        }

        &self.isects
    }

    fn trace_assembly<'b>(
        &'b mut self,
        assembly: &Assembly,
        wrays: &[Ray],
        accel_rays: &mut [AccelRay],
    ) {
        assembly.object_accel.traverse(
            &mut accel_rays[..],
            &assembly.instances[..],
            |inst, rs| {
                // Transform rays if needed
                if let Some((xstart, xend)) = inst.transform_indices {
                    // Push transforms to stack
                    self.xform_stack.push(&assembly.xforms[xstart..xend]);

                    // Do transforms
                    let xforms = self.xform_stack.top();
                    for ray in &mut rs[..] {
                        let id = ray.id;
                        let t = ray.time;
                        ray.update_from_xformed_world_ray(
                            &wrays[id as usize],
                            &lerp_slice(xforms, t),
                        );
                    }
                }

                // Trace rays
                {
                    // This is kind of weird looking, but what we're doing here is
                    // splitting the rays up based on direction if they were
                    // transformed, and not splitting them up if they weren't
                    // transformed.
                    // But to keep the actual tracing code in one place (DRY),
                    // we map both cases to an array slice that contains slices of
                    // ray arrays.  Gah... that's confusing even when explained.
                    // TODO: do this in a way that's less confusing.  Probably split
                    // the tracing code out into a trace_instance() method or
                    // something.
                    let mut tmp = if inst.transform_indices.is_some() {
                        split_rays_by_direction(rs)
                    } else {
                        [
                            &mut rs[..],
                            &mut [],
                            &mut [],
                            &mut [],
                            &mut [],
                            &mut [],
                            &mut [],
                            &mut [],
                        ]
                    };
                    let mut ray_sets = if inst.transform_indices.is_some() {
                        &mut tmp[..]
                    } else {
                        &mut tmp[..1]
                    };

                    // Loop through the split ray slices and trace them
                    for ray_set in ray_sets.iter_mut().filter(|ray_set| !ray_set.is_empty()) {
                        match inst.instance_type {
                            InstanceType::Object => {
                                self.trace_object(
                                    &assembly.objects[inst.data_index],
                                    inst.surface_shader_index.map(
                                        |i| assembly.surface_shaders[i],
                                    ),
                                    wrays,
                                    ray_set,
                                );
                            }

                            InstanceType::Assembly => {
                                self.trace_assembly(
                                    &assembly.assemblies[inst.data_index],
                                    wrays,
                                    ray_set,
                                );
                            }
                        }
                    }
                }

                // Un-transform rays if needed
                if inst.transform_indices.is_some() {
                    // Pop transforms off stack
                    self.xform_stack.pop();

                    // Undo transforms
                    let xforms = self.xform_stack.top();
                    if !xforms.is_empty() {
                        for ray in &mut rs[..] {
                            let id = ray.id;
                            let t = ray.time;
                            ray.update_from_xformed_world_ray(
                                &wrays[id as usize],
                                &lerp_slice(xforms, t),
                            );
                        }
                    } else {
                        for ray in &mut rs[..] {
                            let id = ray.id;
                            ray.update_from_world_ray(&wrays[id as usize]);
                        }
                    }
                }
            },
        );
    }

    fn trace_object<'b>(
        &'b mut self,
        obj: &Object,
        surface_shader: Option<&SurfaceShader>,
        wrays: &[Ray],
        rays: &mut [AccelRay],
    ) {
        match *obj {
            Object::Surface(surface) => {
                let unassigned_shader = SimpleSurfaceShader::Emit {
                    color: XYZ::from_tuple(rec709_to_xyz((1.0, 0.0, 1.0))),
                };
                let shader = surface_shader.unwrap_or(&unassigned_shader);

                surface.intersect_rays(
                    rays,
                    wrays,
                    &mut self.isects,
                    shader,
                    self.xform_stack.top(),
                );
            }

            Object::SurfaceLight(surface) => {
                // Lights don't use shaders
                let bogus_shader = SimpleSurfaceShader::Emit {
                    color: XYZ::from_tuple(rec709_to_xyz((1.0, 0.0, 1.0))),
                };

                surface.intersect_rays(
                    rays,
                    wrays,
                    &mut self.isects,
                    &bogus_shader,
                    self.xform_stack.top(),
                );
            }
        }
    }
}


fn split_rays_by_direction(rays: &mut [AccelRay]) -> [&mut [AccelRay]; 8] {
    // |   |   |   |   |   |   |   |   |
    //     s1  s2  s3  s4  s5  s6  s7
    let s4 = partition(&mut rays[..], |r| r.dir_inv.x() >= 0.0);

    let s2 = partition(&mut rays[..s4], |r| r.dir_inv.y() >= 0.0);
    let s6 = s4 + partition(&mut rays[s4..], |r| r.dir_inv.y() >= 0.0);

    let s1 = partition(&mut rays[..s2], |r| r.dir_inv.z() >= 0.0);
    let s3 = s2 + partition(&mut rays[s2..s4], |r| r.dir_inv.z() >= 0.0);
    let s5 = s4 + partition(&mut rays[s4..s6], |r| r.dir_inv.z() >= 0.0);
    let s7 = s6 + partition(&mut rays[s6..], |r| r.dir_inv.z() >= 0.0);

    let (rest, rs7) = rays.split_at_mut(s7);
    let (rest, rs6) = rest.split_at_mut(s6);
    let (rest, rs5) = rest.split_at_mut(s5);
    let (rest, rs4) = rest.split_at_mut(s4);
    let (rest, rs3) = rest.split_at_mut(s3);
    let (rest, rs2) = rest.split_at_mut(s2);
    let (rs0, rs1) = rest.split_at_mut(s1);

    [rs0, rs1, rs2, rs3, rs4, rs5, rs6, rs7]
}
