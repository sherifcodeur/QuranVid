use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use std::process::{Child, Command, Stdio, ChildStdout};
use std::io::{Read, Write};
use wgpu::util::DeviceExt;
use glyphon::{Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextAtlas, TextArea, TextBounds, Weight, cosmic_text::Align};

pub struct WgpuContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub texture_format: wgpu::TextureFormat,
}

impl WgpuContext {
    pub async fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| format!("Failed to find an appropriate adapter: {:?}", e))?;

        let (device, queue): (wgpu::Device, wgpu::Queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("WGPU Video Export Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(), // Use default limits initially
                    memory_hints: wgpu::MemoryHints::Performance,
                    /* trace: None, experimental_features: ... */
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| format!("Failed to create device: {}", e))?;

        Ok(Self {
            device,
            queue,
            texture_format: wgpu::TextureFormat::Rgba8UnormSrgb, // Standard format for compatibility
        })
    }
}


pub struct VideoDecoder {
    pub child: Child,
    pub width: u32,
    pub height: u32,
    pub reader: std::io::BufReader<ChildStdout>,
}

impl VideoDecoder {
    pub fn new(path: &str, width: u32, height: u32, fps: u32) -> Result<Self, String> {
        let ffmpeg_exe = "ffmpeg"; // Or use resolve_binary logic here
        
        let mut cmd = Command::new(ffmpeg_exe);
        cmd.args(&[
            "-i", path,
            "-f", "image2pipe",
            "-pix_fmt", "rgba", // WGPU compatible format
            "-vcodec", "rawvideo",
            "-r", &fps.to_string(), // Ensure frame rate match
            "-",
        ]);
        
        // Hide window on Windows
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        cmd.stdout(Stdio::piped())
           .stderr(Stdio::piped()); // Capture stderr to avoid buffer filling? Or just null it if not debugging.

        let mut child = cmd.spawn().map_err(|e| format!("Failed to spawn ffmpeg decoder: {}", e))?;
        
        let stdout = child.stdout.take().ok_or("Failed to open stdout")?;
        
        Ok(Self {
            child,
            width,
            height,
            reader: std::io::BufReader::new(stdout),
        })
    }
    
    pub fn read_frame(&mut self) -> Result<Vec<u8>, String> {
        let frame_size = (self.width * self.height * 4) as usize;
        let mut buffer = vec![0u8; frame_size];
        
        self.reader.read_exact(&mut buffer).map_err(|e: std::io::Error| {
             if e.kind() == std::io::ErrorKind::UnexpectedEof {
                 "EOF".to_string()
             } else {
                 format!("Failed to read frame: {}", e)
             }
        })?;
        
        Ok(buffer)
    }
}

pub struct VideoEncoder {
    pub child: Child,
    pub width: u32,
    pub height: u32,
    pub writer: std::io::BufWriter<std::process::ChildStdin>,
}

impl VideoEncoder {
    pub fn new(
        path: &str, 
        w: u32, 
        h: u32, 
        fps: u32, 
        vcodec: &str, 
        vparams: Vec<String>, 
        vpreset: Option<String>,
        audio_paths: &[String],
        start_s: f64,
        duration_s: f64
    ) -> Result<Self, String> {
        let mut command = Command::new("ffmpeg");
        command.args(&[
            "-y",
            "-f", "rawvideo",
            "-vcodec", "rawvideo",
            "-s", &format!("{}x{}", w, h),
            "-pix_fmt", "rgba",
            "-r", &fps.to_string(),
            "-i", "-", // Video input from stdin (index 0)
        ]);

        // Add audio inputs (indexes 1..N)
        for p in audio_paths {
            command.arg("-i").arg(p);
        }

        // Setup filter complex for audio
        let mut filter_complex = String::new();
        let mut have_audio = false;
        
        if !audio_paths.is_empty() {
             let a = audio_paths.len();
             have_audio = true;
             // Audio indices start at 1 (0 is video pipe)
             for j in 0..a {
                 filter_complex.push_str(&format!("[{}:a]aresample=48000[aa{}];", j + 1, j));
             }
             
             let mut ins = String::new();
             for j in 0..a {
                 ins.push_str(&format!("[aa{}]", j));
             }
             
             if a > 1 {
                 filter_complex.push_str(&format!("{}concat=n={}:v=0:a=1[aacat];", ins, a));
                 filter_complex.push_str(&format!("[aacat]atrim=start={:.6},asetpts=PTS-STARTPTS,atrim=end={:.6}[aout]", start_s, duration_s));
             } else {
                 filter_complex.push_str(&format!("[aa0]atrim=start={:.6},asetpts=PTS-STARTPTS,atrim=end={:.6}[aout]", start_s, duration_s));
             }
        }

        if have_audio {
            command.args(&["-filter_complex", &filter_complex]);
            command.args(&["-map", "0:v", "-map", "[aout]"]);
        } else {
            command.args(&["-map", "0:v"]);
        }

        // Video codec and params
        command.args(&["-c:v", vcodec]);
        if let Some(preset) = vpreset {
            command.args(&["-preset", &preset]);
        }
        for p in vparams {
            command.arg(p);
        }

        // Audio codec
        if have_audio {
            command.args(&["-c:a", "aac", "-b:a", "320k", "-ac", "2"]);
        }

        command.arg("-t").arg(format!("{:.6}", duration_s));
        command.arg(path);

        // Hide window on Windows
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        command.stdin(Stdio::piped())
               .stdout(Stdio::null())
               .stderr(Stdio::piped()); // We might want stderr for progress later

        let mut child = command.spawn().map_err(|e| format!("Failed to spawn encoder: {}", e))?;
        let stdin = child.stdin.take().ok_or("Failed to capture encoder stdin")?;

        Ok(Self {
            child,
            width: w,
            height: h,
            writer: std::io::BufWriter::new(stdin),
        })
    }
    
    pub fn write_frame(&mut self, buffer: &[u8]) -> Result<(), String> {
        self.writer.write_all(buffer).map_err(|e| format!("Failed to write frame: {}", e))
    }
    
    pub fn finish(mut self) -> Result<(), String> {
        // Drop writer to close stdin and signal EOF to ffmpeg
        drop(self.writer);
        let status = self.child.wait().map_err(|e| format!("Failed to wait on ffmpeg: {}", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("FFmpeg exited with error: {}", status))
        }
    }
}

pub struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    alpha_buffer: wgpu::Buffer,
    alpha_bind_group: wgpu::BindGroup,
}

impl ImageRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Overlay Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("overlay.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Overlay Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let alpha_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Alpha Bind Group Layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Overlay Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout, &alpha_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Overlay Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let alpha_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Alpha Buffer"),
            contents: bytemuck::cast_slice(&[1.0f32]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let alpha_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Alpha Bind Group"),
            layout: &alpha_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: alpha_buffer.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            alpha_buffer,
            alpha_bind_group,
        }
    }

    pub fn set_alpha(&self, queue: &wgpu::Queue, alpha: f32) {
        queue.write_buffer(&self.alpha_buffer, 0, bytemuck::cast_slice(&[alpha]));
    }

    pub fn render(&self, device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView, sub_view: &wgpu::TextureView) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Overlay Bind Group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(sub_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Overlay Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.set_bind_group(1, &self.alpha_bind_group, &[]);
            render_pass.draw(0..4, 0..1);
        }
        device.poll(wgpu::PollType::Wait { submission_index: Some(queue.submit(Some(encoder.finish()))), timeout: None }).unwrap();
    }
}



pub struct TextRenderer {
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
    pub cache: Cache,
    pub viewport: glyphon::Viewport,
    pub atlas: TextAtlas,
    pub text_renderer: glyphon::TextRenderer,
    pub buffer: Buffer,
}

impl TextRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let text_renderer = glyphon::TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(32.0, 42.0));

        buffer.set_size(&mut font_system, Some(width as f32), Some(height as f32));
        buffer.shape_until_scroll(&mut font_system, false);

        let viewport = glyphon::Viewport::new(device, &cache);

        Self {
            font_system,
            swash_cache,
            cache,
            viewport,
            atlas,
            text_renderer,
            buffer,
        }
    }

    pub fn render(&mut self, text: &str, device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView, width: u32, height: u32) -> Result<(), String> {
        self.buffer.set_text(&mut self.font_system, text, &Attrs::new().family(Family::SansSerif), Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        self.text_renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            [TextArea {
                buffer: &self.buffer,
                left: 10.0,
                top: 10.0,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                },
                default_color: Color::rgb(255, 255, 255),
                custom_glyphs: &[],
            }],
            &mut self.swash_cache,
        ).map_err(|e| format!("Prepare error: {:?}", e))?;

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            
            self.text_renderer.render(&self.atlas, &self.viewport, &mut pass).map_err(|e| format!("{:?}", e))?;
        }
        
        queue.submit(Some(encoder.finish()));
        Ok(())
    }
}

pub struct Renderer {
    ctx: WgpuContext,
    pub width: u32,
    pub height: u32,
    bg_texture: wgpu::Texture,
    bg_view: wgpu::TextureView,
    text_renderer: TextRenderer,
    pub output_buffer: wgpu::Buffer,
    pub image_renderer: ImageRenderer,
    pub sub_texture: wgpu::Texture,
    pub sub_view: wgpu::TextureView,
}

impl Renderer {
    pub async fn new(width: u32, height: u32) -> Result<Self, String> {
        let ctx = WgpuContext::new().await?;
        
        let texture_size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        
        let bg_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Background Texture"),
            size: texture_size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ctx.texture_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, // COPY_SRC for readback test
            view_formats: &[],
        });
        
        let bg_view = bg_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sub_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Subtitle Texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: ctx.texture_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let sub_view = sub_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let text_renderer = TextRenderer::new(&ctx.device, &ctx.queue, ctx.texture_format, width, height);
        let image_renderer = ImageRenderer::new(&ctx.device, ctx.texture_format);

        // Buffer for reading back data
        let output_buffer_size = (width * height * 4) as wgpu::BufferAddress;
        let output_buffer_desc = wgpu::BufferDescriptor {
            size: output_buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            label: Some("Output Buffer"),
            mapped_at_creation: false,
        };
        let output_buffer = ctx.device.create_buffer(&output_buffer_desc);

        Ok(Self {
            ctx,
            width,
            height,
            bg_texture,
            bg_view,
            text_renderer,
            output_buffer,
            image_renderer,
            sub_texture,
            sub_view,
        })
    }

    pub fn upload_background(&self, data: &[u8]) {
        self.ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.bg_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn render_image(&mut self, alpha: f32) {
        self.image_renderer.set_alpha(&self.ctx.queue, alpha);
        self.image_renderer.render(&self.ctx.device, &self.ctx.queue, &self.bg_view, &self.sub_view);
    }

    pub fn upload_subtitle(&self, data: &[u8]) {
        self.ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.sub_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn render_text(&mut self, text: &str) -> Result<(), String> {
        self.text_renderer.render(text, &self.ctx.device, &self.ctx.queue, &self.bg_view, self.width, self.height)
    }

    pub async fn read_frame(&self) -> Result<Vec<u8>, String> {
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.bg_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.width * 4),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            }
        );
        
        let index = self.ctx.queue.submit(Some(encoder.finish()));
        
        let buffer_slice = self.output_buffer.slice(..);
        let (tx, rx) = tokio::sync::oneshot::channel();
        
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            tx.send(result).unwrap();
        });
        
        self.ctx.device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None }).unwrap();
        
        rx.await.map_err(|e| format!("Map async error: {}", e))?
          .map_err(|e| format!("Buffer map error: {}", e))?;
        
        let data = buffer_slice.get_mapped_range();
        let result = data.to_vec();
        
        drop(data);
        self.output_buffer.unmap();
        
        Ok(result)
    }
}
