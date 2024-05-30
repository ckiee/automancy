use std::collections::VecDeque;
use std::f32::consts::FRAC_PI_6;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::{borrow::Cow, time::Duration};

use arboard::{Clipboard, ImageData};
use hashbrown::HashMap;
use image::{EncodableLayout, RgbaImage};
use num::PrimInt;
use ractor::ActorRef;
use tokio::runtime::Runtime;
use tokio::sync::{oneshot, Mutex};
use wgpu::{
    BufferAddress, BufferDescriptor, BufferUsages, Color, CommandEncoderDescriptor,
    ImageCopyBuffer, ImageDataLayout, IndexFormat, LoadOp, Maintain, MapMode, Operations,
    RenderPassColorAttachment, RenderPassDepthStencilAttachment, RenderPassDescriptor,
    SurfaceError, TextureDescriptor, TextureDimension, TextureUsages, TextureViewDescriptor,
    COPY_BUFFER_ALIGNMENT, COPY_BYTES_PER_ROW_ALIGNMENT,
};
use wgpu::{CommandBuffer, StoreOp};

use automancy_defs::slice_group_by::GroupBy;
use automancy_defs::{colors, math};
use automancy_defs::{coord::TileCoord, math::Vec4};
use automancy_defs::{
    glam::vec2,
    rendering::{make_line, GameUBO, InstanceData, LINE_DEPTH},
};
use automancy_defs::{glam::vec3, rendering::PostProcessingUBO};
use automancy_defs::{id::Id, math::get_screen_world_bounding_vec};
use automancy_defs::{
    math::{
        direction_to_angle, lerp_coords_to_pixel, Float, Matrix4, FAR, HEX_GRID_LAYOUT, SQRT_3,
    },
    window,
};
use automancy_resources::data::item::Item;
use automancy_resources::data::{Data, DataMap};
use automancy_resources::ResourceManager;
use yakui::Rect;
use yakui_wgpu::SurfaceInfo;

use crate::{
    camera::Camera, game::RenderUnit, gpu::IndirectInstanceDrawData, gui::YakuiRenderResources,
};
use crate::{
    game::{GameSystemMessage, TransactionRecord, TransactionRecords, TRANSACTION_ANIMATION_SPEED},
    gui::GameElementPaintRef,
};
use crate::{gpu, gui};
use crate::{
    gpu::{
        AnimationMap, GlobalResources, Gpu, RenderResources, SharedResources, NORMAL_CLEAR,
        SCREENSHOT_FORMAT,
    },
    gui::Gui,
};

const UPS: u64 = 60;
const UPDATE_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / UPS);

pub struct Renderer {
    pub gpu: Gpu,
    pub shared_resources: SharedResources,
    pub render_resources: RenderResources,
    pub global_resources: Arc<GlobalResources>,

    render_info_cache: Arc<
        Mutex<
            Option<(
                HashMap<TileCoord, (Id, RenderUnit)>,
                HashMap<TileCoord, DataMap>,
            )>,
        >,
    >,
    render_info_updating: Arc<AtomicBool>,
    transaction_records_cache: Arc<Mutex<TransactionRecords>>,
    transaction_records_updating: Arc<AtomicBool>,

    pub tile_tints: HashMap<TileCoord, Vec4>,
    pub extra_instances: Vec<(InstanceData, Id, ())>,

    pub take_item_animations: HashMap<Item, VecDeque<(Instant, Rect)>>,

    last_update: Option<Instant>,
    last_game_data: Option<IndirectInstanceDrawData<()>>,

    screenshot_clipboard: Clipboard,
}

impl Renderer {
    pub fn new(
        gpu: Gpu,
        shared_resources: SharedResources,
        render_resources: RenderResources,
        global_resources: Arc<GlobalResources>,
    ) -> Self {
        Self {
            gpu,
            shared_resources,
            render_resources,
            global_resources,

            render_info_cache: Arc::new(Default::default()),
            render_info_updating: Arc::new(Default::default()),
            transaction_records_cache: Arc::new(Default::default()),
            transaction_records_updating: Arc::new(Default::default()),

            tile_tints: Default::default(),
            extra_instances: vec![],

            take_item_animations: Default::default(),

            last_update: None,
            last_game_data: None,

            screenshot_clipboard: Clipboard::new().unwrap(),
        }
    }
}

pub fn try_add_animation(
    resource_man: &ResourceManager,
    start_instant: Instant,
    model: Id,
    animation_map: &mut AnimationMap,
) -> bool {
    if !animation_map.contains_key(&model) {
        let elapsed = Instant::now().duration_since(start_instant).as_secs_f32();

        if let Some((_, anims)) = resource_man.all_models.get(&model) {
            let anims = anims
                .iter()
                .map(|anim| {
                    let last = anim.inputs.last().unwrap();
                    let wrapped = elapsed % last;
                    let index = anim.inputs.partition_point(|v| *v < wrapped);

                    (anim.target, anim.outputs[index])
                })
                .collect::<Vec<_>>();

            let anims = anims
                .binary_group_by_key(|v| v.0)
                .map(|v| (v[0].0, v.iter().fold(Matrix4::IDENTITY, |acc, v| acc * v.1)))
                .collect::<HashMap<_, _>>();

            animation_map.insert(model, anims);

            return true;
        } else {
            return false;
        }
    }

    true
}

impl Renderer {
    pub fn render(
        &mut self,
        start_instant: Instant,
        resource_man: Arc<ResourceManager>,
        tokio: &Runtime,
        screenshotting: bool,
        camera: &Camera,
        gui: &mut Gui,
        game: &ActorRef<GameSystemMessage>,
    ) -> Result<(), SurfaceError> {
        let tile_tints = mem::take(&mut self.tile_tints);
        let mut extra_instances = mem::take(&mut self.extra_instances);

        let size = self.gpu.window.inner_size();

        if size.width == 0 || size.height == 0 {
            return Ok(());
        }

        let mut animation_map = AnimationMap::new();
        let camera_pos = camera.get_pos();
        let camera_pos_float = camera_pos.as_vec3();
        let camera_matrix = camera.get_matrix().as_mat4();
        let culling_range = camera.culling_range;

        if !self.render_info_updating.load(Ordering::Relaxed) {
            let cache = self.render_info_cache.clone();
            let updating = self.render_info_updating.clone();
            let game = game.clone();

            updating.store(true, Ordering::Relaxed);

            tokio.spawn(async move {
                let all_data = game
                    .call(GameSystemMessage::GetAllData, None)
                    .await
                    .unwrap()
                    .unwrap();
                let instances = game
                    .call(
                        |reply| GameSystemMessage::GetAllRenderUnits {
                            reply,
                            culling_range,
                        },
                        None,
                    )
                    .await
                    .unwrap()
                    .unwrap();

                *cache.lock().await = Some((instances, all_data));

                updating.store(false, Ordering::Relaxed);
            });
        }

        let render_info_lock = self.render_info_cache.blocking_lock();
        let Some((render_info, all_data)) = render_info_lock.as_ref() else {
            return Ok(());
        };

        {
            if !self.transaction_records_updating.load(Ordering::Relaxed) {
                let cache = self.transaction_records_cache.clone();
                let updating = self.transaction_records_updating.clone();
                let game = game.clone();

                updating.store(true, Ordering::Relaxed);

                tokio.spawn(async move {
                    let result = game
                        .call(GameSystemMessage::GetRecordedTransactions, None)
                        .await
                        .unwrap()
                        .unwrap();

                    *cache.lock().await = result;

                    updating.store(false, Ordering::Relaxed);
                });
            }

            let transaction_records = self.transaction_records_cache.blocking_lock();

            let now = Instant::now();

            for ((source_coord, coord), instants) in transaction_records.iter() {
                if culling_range.is_in_bounds(**source_coord) && culling_range.is_in_bounds(**coord)
                {
                    for (instant, TransactionRecord { stack, .. }) in instants {
                        let duration = now.duration_since(*instant);
                        let t = duration.as_secs_f64() / TRANSACTION_ANIMATION_SPEED.as_secs_f64();

                        let point = lerp_coords_to_pixel(*source_coord, *coord, t as Float);

                        let direction = *coord - *source_coord;
                        let direction = HEX_GRID_LAYOUT.hex_to_world_pos(*direction);
                        let theta = direction_to_angle(direction);

                        let instance = InstanceData::default()
                            .with_model_matrix(
                                Matrix4::from_translation(vec3(
                                    point.x as Float,
                                    point.y as Float,
                                    (FAR + 0.025) as Float,
                                )) * Matrix4::from_rotation_z(theta)
                                    * Matrix4::from_scale(vec3(0.3, 0.3, 0.3)),
                            )
                            .with_light_pos(camera_pos_float, None);
                        let model = resource_man.item_model_or_missing(stack.item.model);

                        extra_instances.push((instance, model, ()));
                    }
                }
            }
        }

        for (coord, data) in all_data {
            let world_coord = HEX_GRID_LAYOUT.hex_to_world_pos(**coord);
            if let Some(Data::Coord(link)) = data.get(&resource_man.registry.data_ids.link) {
                extra_instances.push((
                    InstanceData::default()
                        .with_color_offset(colors::RED.to_linear())
                        .with_light_pos(camera_pos_float, None)
                        .with_model_matrix(make_line(
                            world_coord,
                            HEX_GRID_LAYOUT.hex_to_world_pos(**link),
                        )),
                    resource_man.registry.model_ids.cube1x1,
                    (),
                ));
            }

            if let Some(Data::Id(id)) = data.get(&resource_man.registry.data_ids.item) {
                extra_instances.push((
                    InstanceData::default()
                        .with_light_pos(camera_pos_float, None)
                        .with_model_matrix(
                            Matrix4::from_translation(world_coord.extend(0.1))
                                * Matrix4::from_scale(vec3(0.25, 0.25, 1.0)),
                        ),
                    resource_man.registry.items[id].model,
                    (),
                ))
            }
        }

        for (coord, (id, unit)) in render_info {
            let tile = resource_man.registry.tiles.get(id).unwrap();

            if let Some(theta) = all_data
                .get(coord)
                .and_then(|data| data.get(&resource_man.registry.data_ids.direction))
                .and_then(|direction| {
                    if let Data::Coord(target) = direction {
                        math::tile_direction_to_angle(*target)
                    } else {
                        None
                    }
                })
            {
                if let Data::Color(color) = tile
                    .data
                    .get(&resource_man.registry.data_ids.direction_color)
                    .unwrap_or(&Data::Color(colors::ORANGE))
                {
                    extra_instances.push((
                        InstanceData::default()
                            .with_color_offset(color.to_linear())
                            .with_light_pos(camera_pos_float, None)
                            .with_model_matrix(
                                unit.instance.get_model_matrix()
                                    * Matrix4::from_rotation_z(theta.to_radians())
                                    * Matrix4::from_rotation_z(FRAC_PI_6 * 5.0)
                                    * Matrix4::from_scale(vec3(0.1, SQRT_3, LINE_DEPTH))
                                    * Matrix4::from_translation(vec3(0.0, 0.5, 0.0)),
                            ),
                        resource_man.registry.model_ids.cube1x1,
                        (),
                    ))
                }
            }
        }

        let game_instances = if Instant::now()
            .duration_since(*self.last_update.get_or_insert_with(Instant::now))
            < UPDATE_INTERVAL
        {
            None
        } else {
            self.last_update = Some(Instant::now());

            let mut render_info = render_info.clone();

            let bound = get_screen_world_bounding_vec(
                window::window_size_double(&self.gpu.window),
                camera_pos,
            )
            .as_vec2()
                + vec2(2.0, 2.0);

            let center = camera_pos_float.truncate();

            for coord in culling_range.into_iter() {
                if !render_info.contains_key(&coord) {
                    let pos = HEX_GRID_LAYOUT.hex_to_world_pos(*coord);

                    let d = pos - center;
                    if d.x <= bound.x && d.y <= bound.y {
                        render_info.insert(
                            coord,
                            (
                                resource_man.registry.none,
                                RenderUnit {
                                    instance: InstanceData::default().with_model_matrix(
                                        Matrix4::from_translation(pos.extend(FAR as Float)),
                                    ),
                                    model_override: None,
                                },
                            ),
                        );
                    }
                }
            }

            for (coord, (id, unit)) in render_info.iter_mut() {
                let tile = resource_man.registry.tiles.get(id).unwrap();

                if let Some(theta) = all_data
                    .get(coord)
                    .and_then(|data| data.get(&resource_man.registry.data_ids.direction))
                    .and_then(|direction| {
                        if let Data::Coord(target) = direction {
                            math::tile_direction_to_angle(*target)
                        } else {
                            None
                        }
                    })
                {
                    unit.instance = unit
                        .instance
                        .add_model_matrix(Matrix4::from_rotation_z(theta.to_radians()));
                } else if let Some(Data::Id(inactive)) = tile
                    .data
                    .get(&resource_man.registry.data_ids.inactive_model)
                {
                    unit.model_override = Some(resource_man.tile_model_or_missing(*inactive));
                }
            }

            {
                for (coord, (_, unit)) in &mut render_info.iter_mut() {
                    let mut instance = unit.instance;

                    if let Some(color) = tile_tints.get(coord) {
                        instance = instance.with_color_offset(color.to_array())
                    }

                    instance = instance.with_light_pos(camera_pos_float, None);

                    unit.instance = instance;
                }

                let mut instances = Vec::new();

                for (coord, (id, unit)) in render_info {
                    let model = resource_man
                        .registry
                        .tiles
                        .get(&id)
                        .map(|v| v.model)
                        .unwrap_or(resource_man.registry.model_ids.missing);

                    let model = unit.model_override.unwrap_or(model);

                    try_add_animation(&resource_man, start_instant, model, &mut animation_map);

                    instances.push((
                        unit.instance,
                        model,
                        HEX_GRID_LAYOUT.hex_to_world_pos(*coord),
                    ))
                }

                let camera_pos = camera_pos_float.truncate();
                instances.sort_by(|(.., a), (.., b)| {
                    camera_pos
                        .distance_squared(*a)
                        .total_cmp(&camera_pos.distance_squared(*b))
                });

                Some(
                    instances
                        .into_iter()
                        .rev()
                        .map(|v| (v.0, v.1, ()))
                        .collect::<Vec<_>>(),
                )
            }
        };

        for (_, model, _) in &extra_instances {
            try_add_animation(&resource_man, start_instant, *model, &mut animation_map);
        }

        drop(render_info_lock);

        let r = self.inner_render(
            screenshotting,
            gui,
            resource_man,
            game_instances,
            extra_instances,
            animation_map,
            camera_matrix,
        );

        gui::reset_custom_paint_state();

        r
    }

    fn inner_render(
        &mut self,
        screenshotting: bool,
        gui: &mut Gui,
        resource_man: Arc<ResourceManager>,
        game_instances: Option<Vec<(InstanceData, Id, ())>>,
        extra_instances: Vec<(InstanceData, Id, ())>,
        animation_map: AnimationMap,
        camera_matrix: Matrix4,
    ) -> Result<(), SurfaceError> {
        let size = self.gpu.window.inner_size();

        let mut game_data = game_instances.map(|game_instances| {
            gpu::indirect_instance(&resource_man, game_instances, &animation_map)
        });

        let extra_game_data =
            gpu::indirect_instance(&resource_man, extra_instances, &animation_map);

        let output = self.gpu.surface.get_current_texture()?;

        {
            let output_size = output.texture.size();

            if output_size.width != size.width || output_size.height != size.height {
                return Ok(());
            }
        }

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("Render Encoder"),
            });

        {
            let (extra_instances, extra_matrix_data, extra_draws) = &extra_game_data;

            gpu::create_or_write_buffer(
                &self.gpu.device,
                &self.gpu.queue,
                &mut self
                    .render_resources
                    .extra_objects_resources
                    .instance_buffer,
                bytemuck::cast_slice(extra_instances.as_slice()),
            );

            let (count, draws) = extra_draws;

            let mut indirect_buffer = vec![];
            draws
                .iter()
                .for_each(|v| indirect_buffer.extend_from_slice(v.0.as_bytes()));
            gpu::create_or_write_buffer(
                &self.gpu.device,
                &self.gpu.queue,
                &mut self
                    .render_resources
                    .extra_objects_resources
                    .indirect_buffer,
                indirect_buffer.as_slice(),
            );

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Extra Objects Render Pass"),
                color_attachments: &[
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.game_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Clear(Color::BLACK),
                            store: StoreOp::Store,
                        },
                    }),
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.normal_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Clear(NORMAL_CLEAR),
                            store: StoreOp::Store,
                        },
                    }),
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.model_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Clear(Color::TRANSPARENT),
                            store: StoreOp::Store,
                        },
                    }),
                ],
                depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                    view: &self.shared_resources.depth_texture().1,
                    depth_ops: Some(Operations {
                        load: LoadOp::Clear(1.0),
                        store: StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            if *count > 0 {
                self.gpu.queue.write_buffer(
                    &self.render_resources.extra_objects_resources.uniform_buffer,
                    0,
                    bytemuck::cast_slice(&[GameUBO::new(camera_matrix)]),
                );
                self.gpu.queue.write_buffer(
                    &self
                        .render_resources
                        .extra_objects_resources
                        .matrix_data_buffer,
                    0,
                    bytemuck::cast_slice(extra_matrix_data.as_slice()),
                );

                render_pass.set_pipeline(&self.global_resources.game_pipeline);
                render_pass.set_bind_group(
                    0,
                    &self.render_resources.extra_objects_resources.bind_group,
                    &[],
                );
                render_pass.set_vertex_buffer(0, self.global_resources.vertex_buffer.slice(..));
                render_pass.set_vertex_buffer(
                    1,
                    self.render_resources
                        .extra_objects_resources
                        .instance_buffer
                        .slice(..),
                );
                render_pass.set_index_buffer(
                    self.global_resources.index_buffer.slice(..),
                    IndexFormat::Uint16,
                );

                render_pass.multi_draw_indexed_indirect(
                    &self
                        .render_resources
                        .extra_objects_resources
                        .indirect_buffer,
                    0,
                    *count,
                );
            }
        }

        if let Some(game_data) = game_data.take().or(self.last_game_data.take()) {
            self.last_game_data = Some(game_data);

            let (game_instances, game_matrix_data, game_draws) =
                self.last_game_data.as_ref().unwrap();

            gpu::create_or_write_buffer(
                &self.gpu.device,
                &self.gpu.queue,
                &mut self.render_resources.game_resources.instance_buffer,
                bytemuck::cast_slice(game_instances.as_slice()),
            );

            let (count, draws) = game_draws;

            let mut indirect_buffer = vec![];
            draws
                .iter()
                .for_each(|v| indirect_buffer.extend_from_slice(v.0.as_bytes()));
            gpu::create_or_write_buffer(
                &self.gpu.device,
                &self.gpu.queue,
                &mut self.render_resources.game_resources.indirect_buffer,
                indirect_buffer.as_slice(),
            );

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Game Render Pass"),
                color_attachments: &[
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.game_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load,
                            store: StoreOp::Store,
                        },
                    }),
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.normal_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load,
                            store: StoreOp::Store,
                        },
                    }),
                    Some(RenderPassColorAttachment {
                        view: &self.shared_resources.model_texture().1,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load,
                            store: StoreOp::Store,
                        },
                    }),
                ],
                depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                    view: &self.shared_resources.depth_texture().1,
                    depth_ops: Some(Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            if *count > 0 {
                self.gpu.queue.write_buffer(
                    &self.render_resources.game_resources.uniform_buffer,
                    0,
                    bytemuck::cast_slice(&[GameUBO::new(camera_matrix)]),
                );
                self.gpu.queue.write_buffer(
                    &self.render_resources.game_resources.matrix_data_buffer,
                    0,
                    bytemuck::cast_slice(game_matrix_data.as_slice()),
                );

                render_pass.set_pipeline(&self.global_resources.game_pipeline);
                render_pass.set_bind_group(
                    0,
                    &self.render_resources.game_resources.bind_group,
                    &[],
                );
                render_pass.set_vertex_buffer(0, self.global_resources.vertex_buffer.slice(..));
                render_pass.set_vertex_buffer(
                    1,
                    self.render_resources
                        .game_resources
                        .instance_buffer
                        .slice(..),
                );
                render_pass.set_index_buffer(
                    self.global_resources.index_buffer.slice(..),
                    IndexFormat::Uint16,
                );

                render_pass.multi_draw_indexed_indirect(
                    &self.render_resources.game_resources.indirect_buffer,
                    0,
                    *count,
                );
            }
        }

        {
            self.gpu.queue.write_buffer(
                &self
                    .render_resources
                    .post_processing_resources
                    .uniform_buffer,
                0,
                bytemuck::cast_slice(&[PostProcessingUBO {
                    camera_matrix: camera_matrix.to_cols_array_2d(),
                }]),
            );

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Game Post Processing Render Pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.shared_resources.game_post_processing_texture().1,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.global_resources.post_processing_pipeline);
            render_pass.set_bind_group(
                0,
                self.shared_resources.game_post_processing_bind_group(),
                &[],
            );
            render_pass.set_bind_group(
                1,
                &self
                    .render_resources
                    .post_processing_resources
                    .bind_group_uniform,
                &[],
            );
            render_pass.draw(0..3, 0..1);
        }

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Game Antialiasing Render Pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.shared_resources.game_antialiasing_texture().1,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.global_resources.fxaa_pipeline);
            render_pass.set_bind_group(
                0,
                self.shared_resources.game_antialiasing_bind_group(),
                &[],
            );
            render_pass.draw(0..3, 0..1);
        }

        let custom_gui_commands: CommandBuffer;
        {
            let surface = SurfaceInfo {
                format: self.gpu.config.format,
                sample_count: 4,
                color_attachments: vec![Some(RenderPassColorAttachment {
                    view: &self.shared_resources.gui_texture().1,
                    resolve_target: Some(&self.shared_resources.gui_texture_resolve().1),
                    ops: Operations {
                        load: LoadOp::Clear(Color::TRANSPARENT),
                        store: StoreOp::Store,
                    },
                })],
                depth_format: None,
                depth_attachment: None,
                depth_load_op: None,
            };

            let resources: &mut YakuiRenderResources = &mut (
                resource_man.clone(),
                self.global_resources.clone(),
                self.render_resources.gui_resources.take(),
                surface.format,
                animation_map,
                Some(Default::default()),
                Default::default(),
            );

            {
                let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                    label: Some("yakui Render Pass"),
                    color_attachments: &surface.color_attachments,
                    depth_stencil_attachment: None,
                    ..Default::default()
                });

                custom_gui_commands = gui.renderer.paint_with::<GameElementPaintRef>(
                    &mut gui.yak,
                    &self.gpu.device,
                    &self.gpu.queue,
                    &mut render_pass,
                    surface,
                    resources,
                );
            }

            self.render_resources.gui_resources = resources.2.take();
        };

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Combine Render Pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.shared_resources.first_combine_texture().1,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.global_resources.combine_pipeline);
            render_pass.set_bind_group(0, self.shared_resources.first_combine_bind_group(), &[]);
            render_pass.draw(0..3, 0..1)
        }

        {
            let view = output
                .texture
                .create_view(&TextureViewDescriptor::default());

            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("Present Pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            render_pass.set_pipeline(&self.global_resources.present_pipeline);
            render_pass.set_bind_group(0, self.shared_resources.present_bind_group(), &[]);
            render_pass.draw(0..3, 0..1)
        }

        fn size_align<T: PrimInt>(size: T, alignment: T) -> T {
            ((size + alignment - T::one()) / alignment) * alignment
        }

        let block_size = output.texture.format().block_copy_size(None).unwrap();
        let texture_dim = output.texture.size();
        let buffer_dim = texture_dim.physical_size(output.texture.format());
        let padded_width = size_align(buffer_dim.width * block_size, COPY_BYTES_PER_ROW_ALIGNMENT);

        let screenshot_buffer = if screenshotting {
            let intermediate_texture = self.gpu.device.create_texture(&TextureDescriptor {
                label: Some("Screenshot Intermediate Texture"),
                size: texture_dim,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: SCREENSHOT_FORMAT,
                usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
                view_formats: &[],
            });

            let intermediate_texture_view =
                intermediate_texture.create_view(&TextureViewDescriptor::default());

            {
                let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                    label: Some("Screenshot Intermediate Pass"),
                    color_attachments: &[Some(RenderPassColorAttachment {
                        view: &intermediate_texture_view,
                        resolve_target: None,
                        ops: Operations {
                            load: LoadOp::Load,
                            store: StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });

                render_pass.set_pipeline(&self.global_resources.screenshot_pipeline);
                render_pass.set_bind_group(0, self.shared_resources.present_bind_group(), &[]);
                render_pass.draw(0..3, 0..1);
            }

            let buffer = self.gpu.device.create_buffer(&BufferDescriptor {
                label: Some("Screenshot Buffer"),
                size: size_align(
                    (padded_width * buffer_dim.height) as BufferAddress,
                    COPY_BUFFER_ALIGNMENT,
                ),
                usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            encoder.copy_texture_to_buffer(
                intermediate_texture.as_image_copy(),
                ImageCopyBuffer {
                    buffer: &buffer,
                    layout: ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_width),
                        rows_per_image: Some(buffer_dim.height),
                    },
                },
                buffer_dim,
            );

            Some(buffer)
        } else {
            None
        };

        self.gpu
            .queue
            .submit([custom_gui_commands, encoder.finish()]);

        if let Some(buffer) = screenshot_buffer {
            {
                let slice = buffer.slice(..);

                let (tx, rx) = oneshot::channel();

                slice.map_async(MapMode::Read, move |result| {
                    tx.send(result).unwrap();
                });
                self.gpu.device.poll(Maintain::Wait);
                rx.blocking_recv().unwrap().unwrap();

                let texture_width = (texture_dim.width * block_size) as usize;
                let data = slice.get_mapped_range();
                let mut result = Vec::<u8>::new();
                for chunk in data.chunks_exact(padded_width as usize) {
                    for pixel in chunk[..texture_width].chunks_exact(4) {
                        result.extend(&[pixel[0], pixel[1], pixel[2], 255]);
                    }
                }

                if let Some(image) =
                    RgbaImage::from_vec(texture_dim.width, texture_dim.height, result)
                {
                    self.screenshot_clipboard
                        .set_image(ImageData {
                            width: image.width() as usize,
                            height: image.height() as usize,
                            bytes: Cow::from(image.as_bytes()),
                        })
                        .unwrap();
                }
            }

            buffer.unmap();
        }

        self.gpu.window.pre_present_notify();

        output.present();

        Ok(())
    }
}
