use std;
use std::cell::Cell;
use std::cmp;
use std::cmp::min;
use std::io::{self, Write};
use std::sync::{RwLock, Mutex};

use crossbeam::sync::MsQueue;
use scoped_threadpool::Pool;

use halton;

use accel::{ACCEL_TRAV_TIME, ACCEL_NODE_RAY_TESTS};
use algorithm::partition_pair;
use color::{Color, XYZ, SpectralSample, map_0_1_to_wavelength};
use float4::Float4;
use fp_utils::robust_ray_origin;
use hash::hash_u32;
use hilbert;
use image::Image;
use math::{fast_logit, upper_power_of_two};
use mis::power_heuristic;
use ray::Ray;
use scene::{Scene, SceneLightSample};
use surface;
use timer::Timer;
use tracer::Tracer;
use transform_stack::TransformStack;


#[derive(Debug)]
pub struct Renderer<'a> {
    pub output_file: String,
    pub resolution: (usize, usize),
    pub spp: usize,
    pub seed: u32,
    pub scene: Scene<'a>,
}

#[derive(Debug, Copy, Clone)]
pub struct RenderStats {
    pub trace_time: f64,
    pub accel_traversal_time: f64,
    pub accel_node_visits: u64,
    pub initial_ray_generation_time: f64,
    pub ray_generation_time: f64,
    pub sample_writing_time: f64,
    pub total_time: f64,
}

impl RenderStats {
    fn new() -> RenderStats {
        RenderStats {
            trace_time: 0.0,
            accel_traversal_time: 0.0,
            accel_node_visits: 0,
            initial_ray_generation_time: 0.0,
            ray_generation_time: 0.0,
            sample_writing_time: 0.0,
            total_time: 0.0,
        }
    }

    fn collect(&mut self, other: RenderStats) {
        self.trace_time += other.trace_time;
        self.accel_traversal_time += other.accel_traversal_time;
        self.accel_node_visits += other.accel_node_visits;
        self.initial_ray_generation_time += other.initial_ray_generation_time;
        self.ray_generation_time += other.ray_generation_time;
        self.sample_writing_time += other.sample_writing_time;
        self.total_time += other.total_time;
    }
}

impl<'a> Renderer<'a> {
    pub fn render(
        &self,
        max_samples_per_bucket: u32,
        crop: Option<(u32, u32, u32, u32)>,
        thread_count: u32,
        do_blender_output: bool,
    ) -> (Image, RenderStats) {
        let mut tpool = Pool::new(thread_count);

        let image = Image::new(self.resolution.0, self.resolution.1);
        let (img_width, img_height) = (image.width(), image.height());

        let all_jobs_queued = RwLock::new(false);

        let collective_stats = RwLock::new(RenderStats::new());

        // Set up job queue
        let job_queue = MsQueue::new();

        // For printing render progress
        let pixels_rendered = Mutex::new(Cell::new(0));

        // Calculate dimensions and coordinates of what we're rendering.  This
        // accounts for cropping.
        let (width, height, start_x, start_y) = if let Some((x1, y1, x2, y2)) = crop {
            let x1 = min(x1 as usize, img_width - 1);
            let y1 = min(y1 as usize, img_height - 1);
            let x2 = min(x2 as usize, img_width - 1);
            let y2 = min(y2 as usize, img_height - 1);
            (x2 - x1 + 1, y2 - y1 + 1, x1, y1)
        } else {
            (img_width, img_height, 0, 0)
        };

        // Render
        tpool.scoped(|scope| {
            // Spawn worker tasks
            for _ in 0..thread_count {
                let jq = &job_queue;
                let ajq = &all_jobs_queued;
                let img = &image;
                let pixrenref = &pixels_rendered;
                let cstats = &collective_stats;
                scope.execute(move || {
                    self.render_job(
                        jq,
                        ajq,
                        img,
                        width * height,
                        pixrenref,
                        cstats,
                        do_blender_output,
                    )
                });
            }

            // Print initial 0.00% progress
            print!("0.00%");
            let _ = io::stdout().flush();

            // Determine bucket size based on the per-thread maximum number of samples to
            // calculate at a time.
            let (bucket_w, bucket_h) = {
                let target_pixels_per_bucket = max_samples_per_bucket as f64 / self.spp as f64;
                let target_bucket_dim = if target_pixels_per_bucket.sqrt() < 1.0 {
                    1usize
                } else {
                    target_pixels_per_bucket.sqrt() as usize
                };

                (target_bucket_dim, target_bucket_dim)
            };

            // Populate job queue
            let bucket_n = {
                let bucket_count_x = ((width / bucket_w) + 1) as u32;
                let bucket_count_y = ((height / bucket_h) + 1) as u32;
                let larger = cmp::max(bucket_count_x, bucket_count_y);
                let pow2 = upper_power_of_two(larger);
                pow2 * pow2
            };
            for hilbert_d in 0..bucket_n {
                let (bx, by) = hilbert::d2xy(hilbert_d);

                let x = bx as usize * bucket_w;
                let y = by as usize * bucket_h;
                let w = if width >= x {
                    min(bucket_w, width - x)
                } else {
                    bucket_w
                };
                let h = if height >= y {
                    min(bucket_h, height - y)
                } else {
                    bucket_h
                };
                if x < width && y < height && w > 0 && h > 0 {
                    job_queue.push(BucketJob {
                        x: (start_x + x) as u32,
                        y: (start_y + y) as u32,
                        w: w as u32,
                        h: h as u32,
                    });
                }
            }

            // Mark done queuing jobs
            *all_jobs_queued.write().unwrap() = true;
        });

        // Clear percentage progress print
        print!(
            "\r                \r",
        );

        // Return the rendered image and stats
        return (image, *collective_stats.read().unwrap());
    }

    /// Waits for buckets in the job queue to render and renders them when available.
    fn render_job(
        &self,
        job_queue: &MsQueue<BucketJob>,
        all_jobs_queued: &RwLock<bool>,
        image: &Image,
        total_pixels: usize,
        pixels_rendered: &Mutex<Cell<usize>>,
        collected_stats: &RwLock<RenderStats>,
        do_blender_output: bool,
    ) {
        let mut stats = RenderStats::new();
        let mut timer = Timer::new();
        let mut total_timer = Timer::new();

        let mut paths = Vec::new();
        let mut rays = Vec::new();
        let mut tracer = Tracer::from_assembly(&self.scene.root);
        let mut xform_stack = TransformStack::new();

        // Pre-calculate some useful values related to the image plane
        let cmpx = 1.0 / self.resolution.0 as f32;
        let cmpy = 1.0 / self.resolution.1 as f32;
        let min_x = -1.0;
        let max_x = 1.0;
        let min_y = -(self.resolution.1 as f32 / self.resolution.0 as f32);
        let max_y = self.resolution.1 as f32 / self.resolution.0 as f32;
        let x_extent = max_x - min_x;
        let y_extent = max_y - min_y;

        // Render
        'render_loop: loop {
            paths.clear();
            rays.clear();

            // Get bucket, or exit if no more jobs left
            let bucket: BucketJob;
            loop {
                if let Some(b) = job_queue.try_pop() {
                    bucket = b;
                    break;
                } else if *all_jobs_queued.read().unwrap() {
                    break 'render_loop;
                }
            }

            timer.tick();
            // Generate light paths and initial rays
            for y in bucket.y..(bucket.y + bucket.h) {
                for x in bucket.x..(bucket.x + bucket.w) {
                    let offset = hash_u32(((x as u32) << 16) ^ (y as u32), self.seed);
                    for si in 0..self.spp {
                        // Calculate image plane x and y coordinates
                        let (img_x, img_y) = {
                            let filter_x = fast_logit(get_sample(4, offset + si as u32), 1.5) + 0.5;
                            let filter_y = fast_logit(get_sample(5, offset + si as u32), 1.5) + 0.5;
                            let samp_x = (filter_x + x as f32) * cmpx;
                            let samp_y = (filter_y + y as f32) * cmpy;
                            ((samp_x - 0.5) * x_extent, (0.5 - samp_y) * y_extent)
                        };

                        // Create the light path and initial ray for this sample
                        let (path, ray) = LightPath::new(
                            &self.scene,
                            (x, y),
                            (img_x, img_y),
                            (
                                get_sample(0, offset + si as u32),
                                get_sample(1, offset + si as u32),
                            ),
                            get_sample(2, offset + si as u32),
                            map_0_1_to_wavelength(get_sample(3, offset + si as u32)),
                            offset + si as u32,
                        );
                        paths.push(path);
                        rays.push(ray);
                    }
                }
            }
            stats.initial_ray_generation_time += timer.tick() as f64;

            // Trace the paths!
            let mut pi = paths.len();
            while pi > 0 {
                // Test rays against scene
                let isects = tracer.trace(&rays);
                stats.trace_time += timer.tick() as f64;

                // Determine next rays to shoot based on result
                pi = partition_pair(&mut paths[..pi], &mut rays[..pi], |i, path, ray| {
                    path.next(&mut xform_stack, &self.scene, &isects[i], &mut *ray)
                });
                stats.ray_generation_time += timer.tick() as f64;
            }

            {
                // Calculate color based on ray hits and save to image
                let min = (bucket.x, bucket.y);
                let max = (bucket.x + bucket.w, bucket.y + bucket.h);
                let mut img_bucket = image.get_bucket(min, max);
                for path in &paths {
                    let path_col = SpectralSample::from_parts(path.color, path.wavelength);
                    let mut col = img_bucket.get(path.pixel_co.0, path.pixel_co.1);
                    col += XYZ::from_spectral_sample(&path_col) / self.spp as f32;
                    img_bucket.set(path.pixel_co.0, path.pixel_co.1, col);
                }
                stats.sample_writing_time += timer.tick() as f64;

                // Pre-calculate base64 encoding if needed
                let base64_enc = if do_blender_output {
                    use color::xyz_to_rec709_e;
                    Some(img_bucket.rgba_base64(xyz_to_rec709_e))
                } else {
                    None
                };

                // Print render progress, and image data if doing blender output
                let guard = pixels_rendered.lock().unwrap();
                let mut pr = (*guard).get();
                let percentage_old = pr as f64 / total_pixels as f64 * 100.0;

                pr += bucket.w as usize * bucket.h as usize;
                (*guard).set(pr);
                let percentage_new = pr as f64 / total_pixels as f64 * 100.0;

                let old_string = format!("{:.2}%", percentage_old);
                let new_string = format!("{:.2}%", percentage_new);

                if let Some(bucket_data) = base64_enc {
                    // If doing Blender output
                    println!("DIV");
                    println!("{}", new_string);
                    println!("{} {} {} {}", min.0, min.1, max.0, max.1);
                    println!("{}", bucket_data);
                    println!("BUCKET_END");
                    println!("DIV");
                } else {
                    // If doing console output
                    if new_string != old_string {
                        print!("\r{}", new_string);
                    }
                }
                let _ = io::stdout().flush();
            }
        }

        stats.total_time += total_timer.tick() as f64;
        ACCEL_TRAV_TIME.with(|att| {
            stats.accel_traversal_time = att.get();
            att.set(0.0);
        });
        ACCEL_NODE_RAY_TESTS.with(|anv| {
            stats.accel_node_visits = anv.get();
            anv.set(0);
        });

        // Collect stats
        collected_stats.write().unwrap().collect(stats);
    }
}


#[derive(Debug)]
enum LightPathEvent {
    CameraRay,
    BounceRay,
    ShadowRay,
}

#[derive(Debug)]
pub struct LightPath {
    event: LightPathEvent,
    bounce_count: u32,

    pixel_co: (u32, u32),
    lds_offset: u32,
    dim_offset: Cell<u32>,
    time: f32,
    wavelength: f32,

    next_bounce_ray: Option<Ray>,
    next_attenuation_fac: Float4,

    closure_sample_pdf: f32,
    light_attenuation: Float4,
    pending_color_addition: Float4,
    color: Float4,
}

impl LightPath {
    fn new(
        scene: &Scene,
        pixel_co: (u32, u32),
        image_plane_co: (f32, f32),
        lens_uv: (f32, f32),
        time: f32,
        wavelength: f32,
        lds_offset: u32,
    ) -> (LightPath, Ray) {
        (
            LightPath {
                event: LightPathEvent::CameraRay,
                bounce_count: 0,

                pixel_co: pixel_co,
                lds_offset: lds_offset,
                dim_offset: Cell::new(6),
                time: time,
                wavelength: wavelength,

                next_bounce_ray: None,
                next_attenuation_fac: Float4::splat(1.0),

                closure_sample_pdf: 1.0,
                light_attenuation: Float4::splat(1.0),
                pending_color_addition: Float4::splat(0.0),
                color: Float4::splat(0.0),
            },

            scene.camera.generate_ray(
                image_plane_co.0,
                image_plane_co.1,
                time,
                wavelength,
                lens_uv.0,
                lens_uv.1,
            ),
        )
    }

    fn next_lds_samp(&self) -> f32 {
        let dimension = self.dim_offset.get();
        self.dim_offset.set(dimension + 1);
        get_sample(dimension, self.lds_offset)
    }

    fn next(
        &mut self,
        xform_stack: &mut TransformStack,
        scene: &Scene,
        isect: &surface::SurfaceIntersection,
        ray: &mut Ray,
    ) -> bool {
        match self.event {
            //--------------------------------------------------------------------
            // Result of Camera or bounce ray, prepare next bounce and light rays
            LightPathEvent::CameraRay |
            LightPathEvent::BounceRay => {
                if let surface::SurfaceIntersection::Hit {
                    intersection_data: ref idata,
                    ref closure,
                } = *isect
                {
                    // Hit something!  Do the stuff

                    // If it's an emission closure, handle specially:
                    // - Collect light from the emission.
                    // - Terminate the path.
                    use shading::surface_closure::SurfaceClosureUnion;
                    if let &SurfaceClosureUnion::EmitClosure(ref clsr) = closure {
                        if let LightPathEvent::CameraRay = self.event {
                            self.color += clsr.emitted_color().e;
                        } else {
                            let mis_pdf =
                                power_heuristic(self.closure_sample_pdf, idata.sample_pdf);
                            self.color += clsr.emitted_color().e * self.light_attenuation / mis_pdf;
                        };

                        return false;
                    }

                    // Roll the previous closure pdf into the attenauation
                    self.light_attenuation /= self.closure_sample_pdf;

                    // Prepare light ray
                    let light_n = self.next_lds_samp();
                    let light_uvw = (
                        self.next_lds_samp(),
                        self.next_lds_samp(),
                        self.next_lds_samp(),
                    );
                    xform_stack.clear();
                    let light_info = scene.sample_lights(
                        xform_stack,
                        light_n,
                        light_uvw,
                        self.wavelength,
                        self.time,
                        isect,
                    );
                    let found_light = if light_info.is_none() || light_info.pdf() <= 0.0 ||
                        light_info.selection_pdf() <= 0.0
                    {
                        false
                    } else {
                        let light_pdf = light_info.pdf();
                        let light_sel_pdf = light_info.selection_pdf();
                        let material = closure.as_surface_closure();

                        // Calculate the shadow ray and surface closure stuff
                        let (attenuation, closure_pdf, shadow_ray) = match light_info {
                            SceneLightSample::None => unreachable!(),

                            // Distant light
                            SceneLightSample::Distant { direction, .. } => {
                                let attenuation =
                                    material.evaluate(ray.dir, direction, idata.nor, idata.nor_g);
                                let closure_pdf =
                                    material.sample_pdf(ray.dir, direction, idata.nor, idata.nor_g);
                                let mut shadow_ray = {
                                    // Calculate the shadow ray for testing if the light is
                                    // in shadow or not.
                                    let offset_pos = robust_ray_origin(
                                        idata.pos,
                                        idata.pos_err,
                                        idata.nor_g.normalized(),
                                        direction,
                                    );
                                    Ray::new(
                                        offset_pos,
                                        direction,
                                        self.time,
                                        self.wavelength,
                                        true,
                                    )
                                };
                                shadow_ray.max_t = std::f32::INFINITY;
                                (attenuation, closure_pdf, shadow_ray)
                            }

                            // Surface light
                            SceneLightSample::Surface { sample_geo, .. } => {
                                let dir = sample_geo.0 - idata.pos;
                                let attenuation =
                                    material.evaluate(ray.dir, dir, idata.nor, idata.nor_g);
                                let closure_pdf =
                                    material.sample_pdf(ray.dir, dir, idata.nor, idata.nor_g);
                                let shadow_ray = {
                                    // Calculate the shadow ray for testing if the light is
                                    // in shadow or not.
                                    let offset_pos = robust_ray_origin(
                                        idata.pos,
                                        idata.pos_err,
                                        idata.nor_g.normalized(),
                                        dir,
                                    );
                                    let offset_end = robust_ray_origin(
                                        sample_geo.0,
                                        sample_geo.2,
                                        sample_geo.1.normalized(),
                                        -dir,
                                    );
                                    Ray::new(
                                        offset_pos,
                                        offset_end - offset_pos,
                                        self.time,
                                        self.wavelength,
                                        true,
                                    )
                                };
                                (attenuation, closure_pdf, shadow_ray)
                            }
                        };

                        // If there's any possible contribution, set up for a
                        // light ray.
                        if attenuation.e.h_max() <= 0.0 {
                            false
                        } else {
                            // Calculate and store the light that will be contributed
                            // to the film plane if the light is not in shadow.
                            let light_mis_pdf = power_heuristic(light_pdf, closure_pdf);
                            self.pending_color_addition = light_info.color().e * attenuation.e *
                                self.light_attenuation /
                                (light_mis_pdf * light_sel_pdf);

                            *ray = shadow_ray;

                            true
                        }
                    };

                    // Prepare bounce ray
                    let do_bounce = if self.bounce_count < 2 {
                        self.bounce_count += 1;

                        // Sample material
                        let (dir, filter, pdf) = {
                            let material = closure.as_surface_closure();
                            let u = self.next_lds_samp();
                            let v = self.next_lds_samp();
                            material.sample(idata.incoming, idata.nor, idata.nor_g, (u, v))
                        };

                        // Check if pdf is zero, to avoid NaN's.
                        if (pdf > 0.0) && (filter.e.h_max() > 0.0) {
                            // Account for the additional light attenuation from
                            // this bounce
                            self.next_attenuation_fac = filter.e;
                            self.closure_sample_pdf = pdf;

                            // Calculate the ray for this bounce
                            let offset_pos = robust_ray_origin(
                                idata.pos,
                                idata.pos_err,
                                idata.nor_g.normalized(),
                                dir,
                            );
                            self.next_bounce_ray =
                                Some(Ray::new(offset_pos, dir, self.time, self.wavelength, false));

                            true
                        } else {
                            false
                        }
                    } else {
                        self.next_bounce_ray = None;
                        false
                    };

                    // Book keeping for next event
                    if found_light {
                        self.event = LightPathEvent::ShadowRay;
                        return true;
                    } else if do_bounce {
                        *ray = self.next_bounce_ray.unwrap();
                        self.event = LightPathEvent::BounceRay;
                        self.light_attenuation *= self.next_attenuation_fac;
                        return true;
                    } else {
                        return false;
                    }
                } else {
                    // Didn't hit anything, so background color
                    self.color += scene
                        .world
                        .background_color
                        .to_spectral_sample(self.wavelength)
                        .e * self.light_attenuation /
                        self.closure_sample_pdf;
                    return false;
                }
            }

            //--------------------------------------------------------------------
            // Result of shadow ray from sampling a light
            LightPathEvent::ShadowRay => {
                // If the light was not in shadow, add it's light to the film
                // plane.
                if let surface::SurfaceIntersection::Miss = *isect {
                    self.color += self.pending_color_addition;
                }

                // Set up for the next bounce, if any
                if let Some(ref nbr) = self.next_bounce_ray {
                    *ray = *nbr;
                    self.light_attenuation *= self.next_attenuation_fac;
                    self.event = LightPathEvent::BounceRay;
                    return true;
                } else {
                    return false;
                }
            }
        }
    }
}

/// Gets a sample, using LDS samples for lower dimensions,
/// and switching to random samples at higher dimensions where
/// LDS samples aren't available.
#[inline(always)]
fn get_sample(dimension: u32, i: u32) -> f32 {
    use hash::hash_u32_to_f32;
    if dimension < halton::MAX_DIMENSION {
        halton::sample(dimension, i)
    } else {
        hash_u32_to_f32(dimension, i)
    }
}


#[derive(Debug)]
struct BucketJob {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
