#![allow(dead_code)]

use std::io::{self, Write};
use std::path::Path;
use std::cmp::min;
use std::cell::Cell;
use std::sync::{RwLock, Mutex};
use scoped_threadpool::Pool;
use crossbeam::sync::MsQueue;

use algorithm::partition_pair;
use ray::Ray;
use tracer::Tracer;
use halton;
use math::fast_logit;
use image::Image;
use surface;
use scene::Scene;
use color::{Color, XYZ, SpectralSample, map_0_1_to_wavelength};

#[derive(Debug)]
pub struct Renderer {
    pub output_file: String,
    pub resolution: (usize, usize),
    pub spp: usize,
    pub scene: Scene,
}

impl Renderer {
    pub fn render(&self, thread_count: u32) {
        let mut tpool = Pool::new(thread_count);

        let mut image = Image::new(self.resolution.0, self.resolution.1);
        let (img_width, img_height) = (image.width(), image.height());

        let all_jobs_queued = RwLock::new(false);

        // Pre-calculate some useful values related to the image plane
        let cmpx = 1.0 / self.resolution.0 as f32;
        let cmpy = 1.0 / self.resolution.1 as f32;
        let min_x = -1.0;
        let max_x = 1.0;
        let min_y = -(self.resolution.1 as f32 / self.resolution.0 as f32);
        let max_y = self.resolution.1 as f32 / self.resolution.0 as f32;
        let x_extent = max_x - min_x;
        let y_extent = max_y - min_y;

        // Set up job queue
        let job_queue = MsQueue::new();

        // For printing render progress
        let total_pixels = self.resolution.0 * self.resolution.1;
        let pixels_rendered = Mutex::new(Cell::new(0));
        let pixrenref = &pixels_rendered;

        // Render
        tpool.scoped(|scope| {
            // Spawn worker tasks
            for _ in 0..thread_count {
                let jq = &job_queue;
                let ajq = &all_jobs_queued;
                let img = &image;
                scope.execute(move || {
                    let mut paths = Vec::new();
                    let mut rays = Vec::new();
                    let mut tracer = Tracer::from_assembly(&self.scene.root);

                    loop {
                        paths.clear();
                        rays.clear();

                        // Get bucket, or exit if no more jobs left
                        let bucket: BucketJob;
                        loop {
                            if let Some(b) = jq.try_pop() {
                                bucket = b;
                                break;
                            } else {
                                if *ajq.read().unwrap() == true {
                                    return;
                                }
                            }
                        }

                        // Generate rays
                        for y in bucket.y..(bucket.y + bucket.h) {
                            for x in bucket.x..(bucket.x + bucket.w) {
                                let offset = hash_u32(((x as u32) << 16) ^ (y as u32), 0);
                                for si in 0..self.spp {
                                    // Calculate image plane x and y coordinates
                                    let (img_x, img_y) = {
                                        let filter_x =
                                            fast_logit(halton::sample(4, offset + si as u32), 1.5) +
                                            0.5;
                                        let filter_y =
                                            fast_logit(halton::sample(5, offset + si as u32), 1.5) +
                                            0.5;
                                        let samp_x = (filter_x + x as f32) * cmpx;
                                        let samp_y = (filter_y + y as f32) * cmpy;
                                        ((samp_x - 0.5) * x_extent, (0.5 - samp_y) * y_extent)
                                    };

                                    // Create the light path and initial ray for this sample
                                    let (path, ray) =
                                        LightPath::new(&self.scene,
                                                       (x, y),
                                                       (img_x, img_y),
                                                       (halton::sample(0, offset + si as u32),
                                                        halton::sample(1, offset + si as u32)),
                                                       halton::sample(2, offset + si as u32),
                                                       map_0_1_to_wavelength(
                                                           halton::sample(3, offset + si as u32)
                                                       ),
                                                       offset + si as u32);
                                    paths.push(path);
                                    rays.push(ray);
                                }
                            }
                        }

                        // Trace the paths!
                        let mut pi = paths.len();
                        while pi > 0 {
                            // Test rays against scene
                            let isects = tracer.trace(&rays);

                            // Determine next rays to shoot based on result
                            pi =
                                partition_pair(&mut paths[..pi], &mut rays[..pi], |i, path, ray| {
                                    path.next(&self.scene, &isects[i], &mut *ray)
                                });
                        }

                        // Calculate color based on ray hits and save to image
                        {
                            let min = (bucket.x, bucket.y);
                            let max = (bucket.x + bucket.w, bucket.y + bucket.h);
                            let mut img_bucket = img.get_bucket(min, max);
                            for path in paths.iter() {
                                let mut col = img_bucket.get(path.pixel_co.0, path.pixel_co.1);
                                col += XYZ::from_spectral_sample(&path.color) / self.spp as f32;
                                img_bucket.set(path.pixel_co.0, path.pixel_co.1, col);
                            }
                        }

                        // Print render progress
                        {
                            let guard = pixrenref.lock().unwrap();
                            let mut pr = (*guard).get();
                            let percentage_old = pr as f64 / total_pixels as f64 * 100.0;

                            pr += bucket.w as usize * bucket.h as usize;
                            (*guard).set(pr);
                            let percentage_new = pr as f64 / total_pixels as f64 * 100.0;

                            let old_string = format!("{:.2}%", percentage_old);
                            let new_string = format!("{:.2}%", percentage_new);

                            if new_string != old_string {
                                print!("\r{}", new_string);
                                let _ = io::stdout().flush();
                            }
                        }
                    }
                });
            }

            // Print initial 0.00% progress
            print!("0.00%");
            let _ = io::stdout().flush();

            // Determine bucket size based on a target number of samples
            // per bucket.
            // TODO: make target samples per bucket configurable
            let target_samples_per_bucket = 1usize << 12;
            let (bucket_w, bucket_h) = {
                let target_pixels_per_bucket = target_samples_per_bucket as f64 / self.spp as f64;
                let target_bucket_dim = if target_pixels_per_bucket.sqrt() < 1.0 {
                    1usize
                } else {
                    target_pixels_per_bucket.sqrt() as usize
                };

                (target_bucket_dim, target_bucket_dim)
            };

            // Populate job queue
            for by in 0..((img_height / bucket_h) + 1) {
                for bx in 0..((img_width / bucket_w) + 1) {
                    let x = bx * bucket_w;
                    let y = by * bucket_h;
                    let w = min(bucket_w, img_width - x);
                    let h = min(bucket_h, img_height - y);
                    if w > 0 && h > 0 {
                        job_queue.push(BucketJob {
                            x: x as u32,
                            y: y as u32,
                            w: w as u32,
                            h: h as u32,
                        });
                    }
                }
            }

            // Mark done queuing jobs
            *all_jobs_queued.write().unwrap() = true;
        });


        // Write rendered image to disk
        let _ = image.write_png(Path::new(&self.output_file));

        // End output with a new line
        println!("");
    }
}


#[derive(Debug)]
pub struct LightPath {
    pixel_co: (u32, u32),
    lds_offset: u32,
    dim_offset: u32,
    round: u32,
    time: f32,
    wavelength: f32,
    interaction: surface::SurfaceIntersection,
    light_attenuation: SpectralSample,
    pending_color_addition: SpectralSample,
    color: SpectralSample,
}

impl LightPath {
    fn new(scene: &Scene,
           pixel_co: (u32, u32),
           image_plane_co: (f32, f32),
           lens_uv: (f32, f32),
           time: f32,
           wavelength: f32,
           lds_offset: u32)
           -> (LightPath, Ray) {
        (LightPath {
            pixel_co: pixel_co,
            lds_offset: lds_offset,
            dim_offset: 6,
            round: 0,
            time: time,
            wavelength: wavelength,
            interaction: surface::SurfaceIntersection::Miss,
            light_attenuation: SpectralSample::from_value(1.0, wavelength),
            pending_color_addition: SpectralSample::new(wavelength),
            color: SpectralSample::new(wavelength),
        },

         scene.camera.generate_ray(image_plane_co.0,
                                   image_plane_co.1,
                                   time,
                                   lens_uv.0,
                                   lens_uv.1))
    }

    fn next_lds_samp(&mut self) -> f32 {
        let s = halton::sample(self.dim_offset, self.lds_offset);
        self.dim_offset += 1;
        s
    }

    fn next(&mut self, scene: &Scene, isect: &surface::SurfaceIntersection, ray: &mut Ray) -> bool {
        self.round += 1;

        // Result of shading ray, prepare light ray
        if self.round % 2 == 1 {
            if let &surface::SurfaceIntersection::Hit { t: _,
                                                        incoming: _,
                                                        pos,
                                                        nor,
                                                        local_space: _,
                                                        closure } = isect {
                // Hit something!  Do the stuff
                self.interaction = *isect; // Store interaction for use in next phase

                // Prepare light ray
                let light_n = self.next_lds_samp();
                let light_uvw = (self.next_lds_samp(), self.next_lds_samp(), self.next_lds_samp());
                if let Some((light_color, shadow_vec, light_pdf)) = scene.root
                    .sample_lights(light_n, light_uvw, self.wavelength, self.time, isect) {
                    // Calculate and store the light that will be contributed
                    // to the film plane if the light is not in shadow.
                    self.pending_color_addition = {
                        let material = closure.as_surface_closure();
                        let la = material.evaluate(ray.dir, shadow_vec, nor, self.wavelength);
                        light_color * la * self.light_attenuation / light_pdf
                    };

                    // Calculate the shadow ray for testing if the light is
                    // in shadow or not.
                    // TODO: use proper ray offsets for avoiding self-shadowing
                    // rather than this hacky stupid stuff.
                    *ray = Ray::new(pos + shadow_vec.normalized() * 0.001,
                                    shadow_vec,
                                    self.time,
                                    true);

                    return true;
                } else {
                    return false;
                }
            } else {
                // Didn't hit anything, so background color
                let xyz = XYZ::new(0.0, 0.0, 0.0);
                self.color += xyz.to_spectral_sample(self.wavelength);
                return false;
            }
        }
        // Result of light ray, prepare shading ray
        else if self.round % 2 == 0 {
            // If the light was not in shadow, add it's light to the film
            // plane.
            if let &surface::SurfaceIntersection::Miss = isect {
                self.color += self.pending_color_addition;
            }

            // Calculate bounced lighting!
            if self.round < 6 {
                if let surface::SurfaceIntersection::Hit { t: _,
                                                           pos,
                                                           incoming,
                                                           nor,
                                                           local_space: _,
                                                           closure } = self.interaction {
                    // Sample material
                    let (dir, filter, pdf) = {
                        let material = closure.as_surface_closure();
                        let u = self.next_lds_samp();
                        let v = self.next_lds_samp();
                        material.sample(incoming, nor, (u, v), self.wavelength)
                    };

                    // Account for the additional light attenuation from
                    // this bounce
                    self.light_attenuation *= filter / pdf;

                    // Calculate the ray for this bounce
                    *ray = Ray::new(pos + dir.normalized() * 0.0001, dir, self.time, false);

                    return true;
                } else {
                    return false;
                }
            } else {
                return false;
            }
        } else {
            // TODO
            unimplemented!()
        }
    }
}


#[derive(Debug)]
struct BucketJob {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}


fn hash_u32(n: u32, seed: u32) -> u32 {
    let mut hash = n;

    for _ in 0..3 {
        hash = hash.wrapping_mul(1936502639);
        hash ^= hash.wrapping_shr(16);
        hash = hash.wrapping_add(seed);
    }

    return hash;
}


fn srgb_gamma(n: f32) -> f32 {
    if n < 0.0031308 {
        n * 12.92
    } else {
        (1.055 * n.powf(1.0 / 2.4)) - 0.055
    }
}

fn srgb_inv_gamma(n: f32) -> f32 {
    if n < 0.04045 {
        n / 12.92
    } else {
        ((n + 0.055) / 1.055).powf(2.4)
    }
}
