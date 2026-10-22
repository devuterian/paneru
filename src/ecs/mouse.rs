use bevy::app::{App, Plugin, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::MessageReader;
use bevy::ecs::query::Without;
use bevy::ecs::query::{Has, With};
use bevy::ecs::schedule::IntoScheduleConfigs as _;
use bevy::ecs::system::{Commands, Local, Populated, Query, Res, Single};
use bevy::math::IRect;
use bevy::time::Time;
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

use super::{MouseHeldMarker, Timeout};
use crate::commands::refresh_size_on_display;
use crate::config::Config;
use crate::ecs::focus::PendingFocusLayout;
use crate::ecs::layout::LayoutStrip;
use crate::ecs::params::{GlobalState, Windows};
use crate::ecs::{
    ActiveWorkspaceMarker, DockPosition, MissionControlActive, Position, RepositionMarker,
    Scrolling, SelectedVirtualMarker, SpawnCommandsExt, UpdateSet,
};
use bevy::ecs::schedule::common_conditions::on_message;

use crate::events::{Event, InputEvent};
use crate::manager::{Display, Origin, Window, WindowManager, origin_from};
use crate::platform::WinID;
use crate::util::round_px;

/// Bottom-right corner region (`NxN` pixels) where focus events are suppressed.
/// Sized to a representative macOS title bar height — see karinushka/paneru#233:
/// macOS prevents windows from being moved further down than a fully visible title bar,
/// so the parked sliver of a hidden virtual workspace lives within this region.
const CORNER_DEAD_ZONE_PX: i32 = 30;

/// How far, in pixels, both the cursor and the window have to travel between
/// mouse-down and mouse-up for the release to count as dragging the window to
/// a new place in the strip rather than a click.
const DRAG_REORDER_THRESHOLD_PX: i32 = 24;

/// A size change beyond this many pixels means the window was resized by an
/// edge, not dragged by its title bar.
const DRAG_RESIZE_TOLERANCE_PX: i32 = 2;

/// How far the window's horizontal travel may differ from the cursor's for it
/// to still count as carried along by the cursor. Anything else moved it —
/// paneru scrolling the strip while the button was held down, say.
const DRAG_FOLLOW_TOLERANCE_PX: i32 = 40;

/// Drops a recorded drag start if the matching mouse-up never arrives.
const DRAG_START_TIMEOUT: Duration = Duration::from_secs(60);

/// Where a window was when the mouse button went down on it, so the release
/// can tell a title-bar drag apart from a click or an edge resize.
#[derive(Component)]
pub(super) struct WindowDragStart {
    entity: Entity,
    point: Origin,
    frame: IRect,
    /// The window's frame read straight from the OS on mouse-up. Its move
    /// notifications can trail the release, so the cached one may be stale.
    released: Option<IRect>,
}

/// Strips as a window drop sees them: where they sit, which display owns
/// them, and whether they are the one shown on that display.
type DropStrips<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut LayoutStrip,
        &'static Position,
        &'static ChildOf,
        Option<&'static RepositionMarker>,
        Has<ActiveWorkspaceMarker>,
        Has<SelectedVirtualMarker>,
    ),
>;

/// Windows whose frame a mouse-up re-reads from the OS.
type DraggedWindows<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut Window,
        &'static mut Position,
        Has<RepositionMarker>,
    ),
    Without<LayoutStrip>,
>;

pub struct MouseEventsPlugin;

impl Plugin for MouseEventsPlugin {
    fn build(&self, app: &mut App) {
        let mission_control_inactive = |mission_control: Option<Res<MissionControlActive>>| {
            mission_control.is_none_or(|active| !active.0)
        };

        // `run_if` also skips fetching each system's parameters (Windows
        // queries, config, window manager) when there's no input event, which
        // matters for perf.
        app.add_systems(
            Update,
            (
                (
                    mouse_moved_trigger,
                    mouse_resize_trigger,
                    mouse_down_trigger,
                )
                    .run_if(mission_control_inactive),
                (refresh_dragged_window_frame, mouse_drag_drop_trigger)
                    .chain()
                    .run_if(mission_control_inactive),
                mouse_up_trigger,
                horizontal_warp_mouse_trigger,
            )
                .in_set(UpdateSet::Input)
                .run_if(on_message::<InputEvent>),
        );
    }
}

/// True when `point` sits inside the bottom-right `CORNER_DEAD_ZONE_PX`-sized
/// square of the display's working area (excluding any Dock).
fn is_in_corner_dead_zone(
    point: Origin,
    display: &Display,
    dock: Option<&DockPosition>,
    config: &Config,
) -> bool {
    let bounds = display.actual_display_bounds(dock, config);
    point.x >= bounds.max.x - CORNER_DEAD_ZONE_PX && point.y >= bounds.max.y - CORNER_DEAD_ZONE_PX
}

/// Handles mouse moved events.
///
/// If "focus follows mouse" is enabled, this function finds the window under the cursor and
/// focuses it. It also handles child windows like sheets and drawers to ensure the correct
/// window receives focus.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse moved event.
/// * `windows` - A query for all windows.
/// * `focused_window` - A query for the currently focused window.
/// * `main_cid` - The main connection ID resource.
/// * `config` - The optional configuration resource.
#[allow(clippy::too_many_arguments)]
fn mouse_moved_trigger(
    mut messages: MessageReader<InputEvent>,
    windows: Windows,
    displays: Query<(&Display, Option<&DockPosition>)>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    time: Res<Time>,
    mut global_state: GlobalState,
    mut commands: Commands,
    mut last_find_query: Local<Option<Duration>>,
    pending_focus_layout: Query<(), With<PendingFocusLayout>>,
    moving: Query<Entity, With<RepositionMarker>>,
    active_workspaces: Query<(Entity, &LayoutStrip), With<ActiveWorkspaceMarker>>,
) {
    const FIND_WINDOW_THROTTLE: Duration = Duration::from_millis(50);

    for InputEvent(event) in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_some_and(|modifier| modifier.matches(*modifiers))
        {
            // Resizing is handled by a separate trigger or logic.
            // For now, let's just intercept it here to prevent focus changes during resize.
            continue;
        }

        // Corner dead zone: suppress focus events when the cursor sits in
        // the bottom-right of any display (where hidden virtual workspace
        // slivers park). See is_in_corner_dead_zone for details.
        let cursor = origin_from(*point);
        if displays.iter().any(|(display, dock)| {
            display.bounds().contains(cursor)
                && is_in_corner_dead_zone(cursor, display, dock, &config)
        }) {
            trace!("mouse moved suppressed in corner dead-zone {point:?}");
            continue;
        }

        if !config.focus_follows_mouse() {
            continue;
        }
        if global_state.ffm_flag().is_some() {
            trace!("ffm_window_id > 0");
            continue;
        }

        // Command focus is applied before auto-centering starts in PostUpdate.
        // Until the strip and its windows settle, hit-testing sees intermediate
        // geometry and can undo that focus by selecting the previous window.
        // Mouse-driven focus keeps skip_reshuffle set and remains unaffected.
        if !global_state.skip_reshuffle()
            && (!pending_focus_layout.is_empty()
                || active_workspaces.iter().any(|(entity, strip)| {
                    moving
                        .iter()
                        .any(|moving| moving == entity || strip.contains(moving))
                }))
        {
            trace!("mouse focus suppressed during command-driven slide");
            continue;
        }
        let pointer = origin_from(*point);
        if let Some((focused, _)) = windows.focused()
            && focused.frame().contains(pointer)
        {
            let has_overlap = windows
                .iter()
                .any(|(w, _)| w.id() != focused.id() && w.frame().contains(pointer));
            if !has_overlap {
                trace!("pointer unambiguously inside focused window.");
                continue;
            }
        }

        let now = time.elapsed();
        if let Some(last_time) = *last_find_query
            && now.saturating_sub(last_time) < FIND_WINDOW_THROTTLE
        {
            trace!("find_window_at_point throttled.");
            continue;
        }
        *last_find_query = Some(now);

        let Ok(window_id) = window_manager.find_window_at_point(point) else {
            debug!("can not find window at point {point:?}");
            continue;
        };
        if windows
            .focused()
            .is_some_and(|(window, _)| window.id() == window_id)
        {
            trace!("allready focused {window_id}");
            continue;
        }
        let Some((window, entity)) = windows.find(window_id) else {
            trace!("can not find focused window: {window_id}");
            continue;
        };

        let child_window = window_manager
            .get_associated_windows(window_id)
            .into_iter()
            .find_map(|child_wid| {
                windows.find(child_wid).and_then(|(window, _)| {
                    window
                        .child_role()
                        .inspect_err(|err| {
                            warn!("getting role {window_id}: {err}");
                        })
                        .is_ok_and(|child| child)
                        .then_some(window)
                })
            });
        if let Some(child) = child_window {
            debug!("found child of {}: {}", child.id(), window.id());
        }

        // Do not reshuffle windows due to moved mouse focus.
        global_state.set_skip_reshuffle(true);
        global_state.set_ffm_flag(Some(window.id()));
        commands.focus_entity(entity, false);
    }
}

/// Handles mouse down events.
///
/// This function finds the window at the click point. If the window is not fully visible,
/// it triggers a reshuffle to expose it.
///
/// # Arguments
///
/// * `trigger` - The Bevy event trigger containing the mouse down event.
/// * `windows` - A query for all windows.
/// * `active_display` - A query for the active display.
/// * `main_cid` - The main connection ID resource.
/// * `commands` - Bevy commands to trigger a reshuffle.
#[allow(clippy::too_many_arguments)]
fn mouse_down_trigger(
    mut messages: MessageReader<InputEvent>,
    windows: Windows,
    active_workspace: Query<(Entity, Option<&Scrolling>), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mouse_held: Query<Entity, With<MouseHeldMarker>>,
    drag_starts: Query<Entity, With<WindowDragStart>>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseDown { point, .. } = event else {
            continue;
        };
        trace!("{point:?}");

        // A new press supersedes any drag whose mouse-up was lost.
        for stale in &drag_starts {
            if let Ok(mut entity_commands) = commands.get_entity(stale) {
                entity_commands.try_despawn();
            }
        }

        let Some((_, entity)) = window_manager
            .find_window_at_point(point)
            .ok()
            .and_then(|window_id| windows.find(window_id))
        else {
            continue;
        };

        // Stop any ongoing scroll.
        for (entity, scroll) in active_workspace {
            if scroll.is_some()
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_remove::<Scrolling>();
            }
        }

        // Clean up any stale marker from a previous click.
        for held in &mouse_held {
            if let Ok(mut entity_commands) = commands.get_entity(held) {
                entity_commands.try_despawn();
            }
        }

        if let Some(frame) = windows.frame(entity) {
            let timeout = Timeout::new(DRAG_START_TIMEOUT, None, &mut commands);
            commands.spawn((
                WindowDragStart {
                    entity,
                    point: origin_from(*point),
                    frame,
                    released: None,
                },
                timeout,
            ));
        }

        if config.window_hidden_ratio() >= 1.0 {
            // At max hidden ratio, never reshuffle on click.
        } else {
            // Defer reshuffle until mouse-up so the window doesn't shift
            // mid-click. The Timeout auto-despawns if mouse-up is lost.
            let timeout = Timeout::new(Duration::from_secs(5), None, &mut commands);
            commands.spawn((MouseHeldMarker(entity), timeout));
        }
    }
}

/// Handles mouse-up events. Triggers the deferred reshuffle so the clicked
/// window slides into view after the user releases the button.
fn mouse_up_trigger(
    mut messages: MessageReader<InputEvent>,
    mouse_held: Query<(Entity, &MouseHeldMarker)>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        if !matches!(event, Event::MouseUp { .. }) {
            continue;
        }

        for (held_entity, marker) in &mouse_held {
            commands.reshuffle_around(marker.0);
            if let Ok(mut entity_commands) = commands.get_entity(held_entity) {
                entity_commands.try_despawn();
            }
        }
    }
}

/// True when the window moved along with the cursor — both by at least
/// [`DRAG_REORDER_THRESHOLD_PX`] — while keeping its size, i.e. it was dragged
/// by its title bar rather than clicked or resized by an edge.
fn is_window_drag(start: &WindowDragStart, cursor: Origin, frame: IRect) -> bool {
    let travel = |from: Origin, to: Origin| (to.x - from.x).abs().max((to.y - from.y).abs());
    let resized = (frame.width() - start.frame.width()).abs() > DRAG_RESIZE_TOLERANCE_PX
        || (frame.height() - start.frame.height()).abs() > DRAG_RESIZE_TOLERANCE_PX;
    // Only the horizontal travel has to match: macOS stops a window at the
    // menu bar while the cursor carries on upwards.
    let followed = ((frame.min.x - start.frame.min.x) - (cursor.x - start.point.x)).abs()
        <= DRAG_FOLLOW_TOLERANCE_PX;
    !resized
        && followed
        && travel(start.point, cursor) >= DRAG_REORDER_THRESHOLD_PX
        && travel(start.frame.min, frame.min) >= DRAG_REORDER_THRESHOLD_PX
}

/// On mouse-up, reads the dragged window's frame from the OS rather than
/// trusting the last move notification, and records where it was let go.
fn refresh_dragged_window_frame(
    mut messages: MessageReader<InputEvent>,
    mut drag_starts: Populated<&mut WindowDragStart>,
    mut windows: DraggedWindows,
) {
    if !messages
        .read()
        .any(|InputEvent(event)| matches!(event, Event::MouseUp { .. }))
    {
        return;
    }
    for mut drag in &mut drag_starts {
        let Ok((mut window, mut position, repositioning)) = windows.get_mut(drag.entity) else {
            continue;
        };
        // Our own move in flight: the frame is ours, not where it was dropped.
        if repositioning {
            continue;
        }
        let Ok(frame) = window.update_frame() else {
            continue;
        };
        if position.0 != frame.min {
            position.0 = frame.min;
        }
        drag.released = Some(frame);
    }
}

/// Handles the release of a window dragged by its title bar. macOS has already
/// moved the window along with the cursor; here it is slotted into the strip
/// at the column under the cursor, the other columns shifting to make room. A
/// release over another display moves the window into that display's strip
/// instead. Floating windows, edge resizes and plain clicks are left alone.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn mouse_drag_drop_trigger(
    mut messages: MessageReader<InputEvent>,
    drag_starts: Populated<(Entity, &WindowDragStart)>,
    windows: Windows,
    mut strips: DropStrips,
    displays: Query<(Entity, &Display, Option<&DockPosition>)>,
    repositioning: Query<(), With<RepositionMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseUp { point, .. } = event else {
            continue;
        };
        let cursor = origin_from(*point);

        for (drag_entity, drag) in &drag_starts {
            if let Ok(mut entity_commands) = commands.get_entity(drag_entity) {
                entity_commands.try_despawn();
            }

            let entity = drag.entity;
            // A window paneru itself is moving did not follow the cursor.
            if repositioning.contains(entity) {
                continue;
            }
            if windows
                .get_managed(entity)
                .is_none_or(|(_, _, unmanaged)| unmanaged.is_some())
            {
                continue;
            }
            let Some(frame) = drag.released.or_else(|| windows.frame(entity)) else {
                continue;
            };
            if !is_window_drag(drag, cursor, frame) {
                continue;
            }
            let Some((source_entity, source_display)) = strips
                .iter()
                .find(|(_, strip, ..)| strip.contains(entity))
                .map(|(strip_entity, _, _, child, ..)| (strip_entity, child.parent()))
            else {
                continue;
            };

            let target_display = displays
                .get(source_display)
                .ok()
                .filter(|(_, display, _)| display.bounds().contains(cursor))
                .or_else(|| {
                    displays
                        .iter()
                        .find(|(_, display, _)| display.bounds().contains(cursor))
                });
            let get_frame = |entity| windows.frame(entity);

            match target_display {
                Some((target_display, display, _)) if target_display != source_display => {
                    let Ok(space_id) = window_manager.active_display_space(display.id()) else {
                        continue;
                    };
                    let Some(target_entity) = strips.iter().find_map(
                        |(strip_entity, strip, _, child, _, active, selected)| {
                            (child.parent() == target_display
                                && (active || selected)
                                && strip.id() == space_id
                                && !strip.is_fullscreen())
                            .then_some(strip_entity)
                        },
                    ) else {
                        continue;
                    };

                    // Keep the width relative to the usable viewport, like
                    // moving the window there with the keyboard does.
                    let width_ratio = displays
                        .get(source_display)
                        .ok()
                        .map(|(_, display, dock)| {
                            display.actual_display_bounds(dock, &config).width()
                        })
                        .filter(|width| *width > 0)
                        .map(|width| f64::from(frame.width()) / f64::from(width));

                    // macOS carries a native tab group as one window, so the
                    // whole group moves.
                    let mut group = vec![entity];
                    if let Ok((_, mut source, ..)) = strips.get_mut(source_entity) {
                        group = source.tab_group(entity).unwrap_or(group);
                        let neighbour = source
                            .left_neighbour(entity)
                            .or_else(|| source.right_neighbour(entity))
                            .filter(|neighbour| !group.contains(neighbour));
                        for member in &group {
                            source.remove(*member);
                        }
                        if let Some(neighbour) = neighbour {
                            commands.reshuffle_around(neighbour);
                        }
                    }
                    if let Ok((_, mut target, Position(origin), _, reposition, ..)) =
                        strips.get_mut(target_entity)
                    {
                        let origin = reposition.map_or(*origin, |target| target.0);
                        let index = target.drop_index(None, cursor.x - origin.x, &get_frame);
                        debug!("dropping {entity} onto display {target_display} at column {index}");
                        target.insert_tab_group_at(index, &group);
                    }
                    commands.reshuffle_around(entity);
                    for member in group {
                        refresh_size_on_display(member, target_display, width_ratio, &mut commands);
                    }
                }
                _ => {
                    let Ok((_, mut strip, Position(origin), _, reposition, ..)) =
                        strips.get_mut(source_entity)
                    else {
                        continue;
                    };
                    // Columns are laid out against where the strip is
                    // heading, not where it is mid-animation.
                    let strip_origin = reposition.map_or(*origin, |target| target.0);
                    if strip.is_fullscreen() {
                        continue;
                    }
                    let Ok(from) = strip.index_of(entity) else {
                        continue;
                    };
                    let to = strip.drop_index(Some(from), cursor.x - strip_origin.x, &get_frame);
                    if to != from {
                        debug!("dragged {entity} from column {from} to {to}");
                        strip.move_column(from, to);
                    }
                    commands.ensure_visible(entity);
                }
            }

            // macOS left the window wherever it was let go. Its slot may not
            // have changed, so mark it as moved to have it laid out again.
            if let Some(layout_position) = windows.layout_position(entity)
                && let Ok(mut entity_commands) = commands.get_entity(entity)
            {
                entity_commands.try_insert(layout_position.clone());
            }
        }
    }
}

#[derive(Default)]
pub(super) struct MouseResizeState {
    last_point: Option<Origin>,
    window_id: Option<WinID>,
}

fn mouse_resize_trigger(
    mut messages: MessageReader<InputEvent>,
    windows: Windows,
    active_workspace: Single<(Entity, &LayoutStrip, &Position), With<ActiveWorkspaceMarker>>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<MouseResizeState>,
    mut commands: Commands,
) {
    for InputEvent(event) in messages.read() {
        let Event::MouseMoved { point, modifiers } = event else {
            continue;
        };

        if config
            .mouse_resize_modifier()
            .is_none_or(|modifier| !modifier.matches(*modifiers))
        {
            state.last_point = None;
            state.window_id = None;
            continue;
        }
        let pointer = origin_from(*point);

        let Some(last_point) = state.last_point else {
            state.last_point = Some(pointer);
            continue;
        };
        state.last_point = Some(pointer);

        let dx = (pointer.x - last_point.x) * 5;
        if dx.abs() < 1 {
            continue;
        }

        let window_id = if let Some(window_id) = state.window_id {
            window_id
        } else {
            let Ok(window_id) = window_manager.find_window_at_point(point) else {
                continue;
            };
            state.window_id = Some(window_id);
            window_id
        };

        let Some((window, entity)) = windows.find(window_id) else {
            continue;
        };
        let (strip_entity, strip, strip_position) = *active_workspace;
        let floating = !strip.contains(entity);

        let mut frame = window.frame();
        let center = frame.center();

        if pointer.x < center.x {
            if floating && let Some(mut origin) = windows.origin(entity) {
                // For floating windows, move the window itself.
                origin.x += dx;
                commands.reposition_entity(entity, origin);
            } else {
                // Resize Left Edge: increase/decrease width AND shift the strip so the right edge stays
                // anchored.
                let mut origin = strip_position.0;
                origin.x += dx;
                commands.reposition_entity(strip_entity, origin);
            }

            frame.min.x += dx;
        } else {
            frame.max.x += dx;
        }
        commands.resize_entity(entity, frame.size());
    }
}

#[derive(Default)]
pub(super) struct WarpVelocityState {
    last: Option<(Origin, Instant)>,
}

fn horizontal_warp_mouse_trigger(
    mut messages: MessageReader<InputEvent>,
    displays: Query<&Display>,
    window_manager: Res<WindowManager>,
    config: Res<Config>,
    mut state: Local<WarpVelocityState>,
) {
    const EDGE_THRESHOLD: i32 = 3;
    /// Inset from the destination display's edge so the cursor doesn't land
    /// directly on the threshold and immediately re-warp back.
    const LANDING_INSET: i32 = 6;
    /// Extrapolate pre-warp horizontal motion by this duration so the cursor
    /// does not feel like it starts from rest on the target display.
    const CARRY_DURATION: Duration = Duration::from_millis(30);
    /// Cap on how far the carry-over can push past the inset, in pixels.
    const MAX_CARRY_PX: i32 = 80;
    /// Stale velocity samples (e.g. from a prior gesture) shouldn't carry.
    const VELOCITY_FRESHNESS: Duration = Duration::from_millis(80);

    for InputEvent(event) in messages.read() {
        let Event::MouseMoved { point, .. } = event else {
            continue;
        };

        let now = Instant::now();
        let point = origin_from(*point);

        // Compute velocity from the previous sample before deciding whether to
        // warp, then refresh the sample so subsequent events build on this one.
        let velocity_x = state.last.and_then(|(prev, t)| {
            let dt = now.saturating_duration_since(t);
            if dt.is_zero() || dt > VELOCITY_FRESHNESS {
                return None;
            }
            let dx = f64::from(point.x - prev.x);
            Some(dx / dt.as_secs_f64())
        });
        state.last = Some((point, now));

        let Some(warp_direction) = config.horizontal_mouse_warp() else {
            return;
        };
        if displays.count() < 2 {
            return;
        }

        let Some(current_display) = displays
            .iter()
            .find(|display| display.bounds().contains(point))
        else {
            return;
        };

        let on_left_edge = (point.x - current_display.bounds().min.x).abs() < EDGE_THRESHOLD;
        let on_right_edge = (current_display.bounds().max.x - point.x).abs() < EDGE_THRESHOLD;
        if !on_left_edge && !on_right_edge {
            return;
        }

        let mut target_displays = displays
            .iter()
            .filter(|display| {
                let above = display.bounds().min.y < current_display.bounds().min.y;
                let below = display.bounds().min.y > current_display.bounds().min.y;
                if on_left_edge {
                    if warp_direction > 0 { below } else { above }
                } else if warp_direction > 0 {
                    above
                } else {
                    below
                }
            })
            .collect::<Vec<_>>();

        target_displays
            .sort_by_key(|display| (display.bounds().min.y - current_display.bounds().min.y).abs());
        let Some(warp_to) = target_displays.first() else {
            return;
        };
        let target = warp_to.bounds();

        // Land at the *opposite* edge so the cursor flow is continuous: leaving
        // the right edge appears at the left edge of the target, and vice versa.
        // Carry over horizontal velocity so the cursor does not feel "stuck" at
        // the edge — extrapolate motion forward into the target display.
        let carry = velocity_x
            .map_or(0, |v| round_px(v * CARRY_DURATION.as_secs_f64()))
            .clamp(-MAX_CARRY_PX, MAX_CARRY_PX);
        let target_x = if on_left_edge {
            // Cursor was moving leftward; carry is negative. Push further from
            // the right edge of the target.
            (target.max.x - LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        } else {
            // Cursor was moving rightward; carry is positive. Push further from
            // the left edge of the target.
            (target.min.x + LANDING_INSET + carry).clamp(target.min.x + 1, target.max.x - 1)
        };

        // Preserve relative Y offset from the source display's top so vertical
        // motion feels continuous (matches macOS's behavior for side-by-side
        // displays). Apply the configured offset signed by warp direction:
        // positive offset pushes the cursor lower when warping downward, and
        // raises it when warping upward — matching the user's physical desk
        // arrangement (e.g. monitor sitting below the laptop).
        // If the equivalent position falls outside the target's Y range (e.g. a
        // tall portrait monitor's bottom region maps off a shorter laptop's
        // bottom), skip the warp — matches macOS native side-by-side behavior
        // where the cursor can only cross at Y values where both displays exist.
        let relative_y = point.y - current_display.bounds().min.y;
        let direction_sign = if target.min.y > current_display.bounds().min.y {
            1
        } else {
            -1
        };
        let signed_offset = config.horizontal_mouse_warp_offset() * direction_sign;
        let target_y = target.min.y + relative_y + signed_offset;
        if target_y < target.min.y || target_y >= target.max.y {
            return;
        }

        let landing = Origin::new(target_x, target_y);
        window_manager.warp_mouse(landing);
        // Reset the velocity sample to the landing point so the next motion
        // event computes velocity from the new position, not the pre-warp one.
        state.last = Some((landing, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ecs::DockPosition;
    use crate::manager::{Display, Origin};
    use bevy::math::IRect;

    fn make_display() -> Display {
        // 1024x768 test display with a 20px menubar, mirrors the values in src/tests.rs.
        Display::new(
            1,
            IRect {
                min: Origin::new(0, 0),
                max: Origin::new(1024, 768),
            },
            20,
        )
    }

    #[test]
    fn window_drag_needs_the_window_to_follow_the_cursor() {
        let start = WindowDragStart {
            entity: Entity::PLACEHOLDER,
            point: Origin::new(100, 10),
            frame: IRect::new(40, 0, 440, 700),
            released: None,
        };
        let moved = |dx, dy| IRect::new(40 + dx, dy, 440 + dx, 700 + dy);

        // Carried along: horizontally, and upwards past a menu bar stop.
        assert!(is_window_drag(&start, Origin::new(400, 10), moved(300, 0)));
        assert!(is_window_drag(
            &start,
            Origin::new(130, -600),
            moved(30, -560)
        ));
        // A click, or a nudge under the threshold.
        assert!(!is_window_drag(&start, Origin::new(100, 10), moved(0, 0)));
        assert!(!is_window_drag(&start, Origin::new(110, 15), moved(10, 5)));
        // Something else moved the window while the cursor went elsewhere.
        assert!(!is_window_drag(
            &start,
            Origin::new(100, 300),
            moved(300, 0)
        ));
        // Pulled by its left edge: origin and width change together.
        assert!(!is_window_drag(
            &start,
            Origin::new(-200, 10),
            IRect::new(-260, 0, 440, 700)
        ));
    }

    #[test]
    fn corner_dead_zone_no_dock() {
        let display = make_display();
        let config = Config::default();

        // Inside the 30x30 bottom-right corner.
        assert!(is_in_corner_dead_zone(
            Origin::new(1000, 750),
            &display,
            None,
            &config
        ));
        assert!(is_in_corner_dead_zone(
            Origin::new(1024, 768),
            &display,
            None,
            &config
        ));

        // Just outside the corner (one pixel above/left).
        assert!(!is_in_corner_dead_zone(
            Origin::new(993, 750),
            &display,
            None,
            &config
        ));
        assert!(!is_in_corner_dead_zone(
            Origin::new(1000, 737),
            &display,
            None,
            &config
        ));
    }

    #[test]
    fn corner_dead_zone_with_bottom_dock() {
        let display = make_display();
        let config = Config::default();
        let dock = DockPosition::Bottom(80);

        // With an 80px dock at the bottom, actual_display_bounds.max.y = 768 - 80 = 688.
        // Corner zone is now y >= 658.
        assert!(is_in_corner_dead_zone(
            Origin::new(1000, 680),
            &display,
            Some(&dock),
            &config
        ));
        // Just outside the corner zone (point within display bounds but outside corner).
        assert!(!is_in_corner_dead_zone(
            Origin::new(1000, 657),
            &display,
            Some(&dock),
            &config
        ));
    }
}
