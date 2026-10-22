use std::cell::Cell;
use std::rc::Rc;

use bevy::prelude::*;
use objc2_core_foundation::CGPoint;

use crate::commands::Command;
use crate::config::{Config, MainOptions, WindowParams};
use crate::ecs::layout::LayoutStrip;
use crate::ecs::{ActiveWorkspaceMarker, Unmanaged};
use crate::events::Event;
use crate::manager::{Origin, Size, Window};
use crate::platform::{Modifiers, WinID};
use crate::{assert_not_on_workspace, assert_on_workspace};

use super::*;

/// Window ids of the active strip's columns, left to right.
fn strip_order(world: &mut World) -> Vec<WinID> {
    let columns = world
        .query_filtered::<&LayoutStrip, With<ActiveWorkspaceMarker>>()
        .single(world)
        .expect("active strip")
        .all_columns();
    columns
        .into_iter()
        .map(|entity| world.get::<Window>(entity).expect("window").id())
        .collect()
}

fn window_frame(world: &mut World, id: WinID) -> IRect {
    let entity = find_window_entity(id, world);
    world.get::<Window>(entity).expect("window").frame()
}

fn point(origin: Origin) -> CGPoint {
    CGPoint::new(origin.x.into(), origin.y.into())
}

/// How the window under test is handled between mouse-down and mouse-up.
#[derive(Clone, Copy)]
enum Gesture {
    /// macOS drags the window along with the cursor by `(dx, dy)`.
    Drag(i32, i32),
    /// Like `Drag`, but the OS never reports the move before the release.
    SilentDrag(i32, i32),
    /// The window moves by `window` while the cursor travels by `cursor`, as
    /// when something other than the cursor moved it.
    Unrelated { window: Origin, cursor: Origin },
    /// The window's left edge is pulled left by `dx`: origin and width change.
    ResizeLeft(i32),
}

impl Gesture {
    fn cursor_travel(self) -> Origin {
        match self {
            Gesture::Drag(dx, dy) | Gesture::SilentDrag(dx, dy) => Origin::new(dx, dy),
            Gesture::Unrelated { cursor, .. } => cursor,
            Gesture::ResizeLeft(dx) => Origin::new(-dx, 0),
        }
    }
}

/// Presses on window `id`'s title bar, applies `gesture`, releases, and lets
/// the layout settle. `verify` sees the world afterwards.
fn run_gesture<F>(harness: TestHarness, id: WinID, gesture: Gesture, verify: F)
where
    F: FnMut(&mut World, MockState) + 'static,
{
    let start = Rc::new(Cell::new(Origin::ZERO));
    let frame_start = Rc::new(Cell::new(IRect::default()));
    let commands = vec![
        Event::MenuOpened { window_id: 0 },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
        Event::Command {
            command: Command::PrintState,
        },
    ];

    let (start_down, frame_down) = (start.clone(), frame_start.clone());
    let start_up = start.clone();
    let mut harness = harness
        .on_iteration(1, move |world, _state| {
            let frame = window_frame(world, id);
            let grab = Origin::new(frame.min.x + 60, frame.min.y + 10);
            start_down.set(grab);
            frame_down.set(frame);
            world.write_message(Event::MouseDown {
                point: point(grab),
                modifiers: Modifiers::empty(),
            });
        })
        .on_iteration(2, move |_world, state| {
            let frame = frame_start.get();
            match gesture {
                Gesture::Drag(dx, dy) => {
                    state.os_move_window(id, frame.min + Origin::new(dx, dy));
                }
                Gesture::SilentDrag(dx, dy) => state.update_window(id, |window| {
                    let size = window.frame.size();
                    window.frame.min = frame.min + Origin::new(dx, dy);
                    window.frame.max = window.frame.min + size;
                }),
                Gesture::Unrelated { window, .. } => {
                    state.os_move_window(id, frame.min + window);
                }
                Gesture::ResizeLeft(dx) => {
                    state.os_move_window(id, frame.min - Origin::new(dx, 0));
                    state.os_resize_window(id, frame.size() + Size::new(dx, 0));
                }
            }
        })
        .on_iteration(3, move |world, _state| {
            let release = start_up.get() + gesture.cursor_travel();
            world.write_message(Event::MouseUp {
                point: point(release),
                modifiers: Modifiers::empty(),
            });
        })
        .on_iteration(4, verify);
    harness.run(commands);
}

#[test]
fn dragging_a_window_past_its_neighbour_reorders_the_strip() {
    run_gesture(
        TestHarness::new().with_windows(3),
        0,
        // Grabbed 60px in, so the cursor passes window 1's center (600).
        Gesture::Drag(560, 0),
        |world, _state| {
            assert_eq!(strip_order(world), vec![1, 0, 2]);
            // The dropped window is laid out in its new slot, right of 1.
            let moved = window_frame(world, 0);
            let neighbour = window_frame(world, 1);
            assert_eq!(moved.min.x, neighbour.max.x);
        },
    );
}

#[test]
fn dragging_a_window_to_the_front_reorders_the_strip() {
    run_gesture(
        TestHarness::new().with_windows(3),
        2,
        Gesture::Drag(-700, 30),
        |world, _state| {
            assert_eq!(strip_order(world), vec![2, 0, 1]);
        },
    );
}

#[test]
fn a_short_drag_keeps_the_order_and_snaps_back() {
    let harness = TestHarness::new().with_windows(3);
    run_gesture(harness, 0, Gesture::Drag(150, 40), |world, _state| {
        assert_eq!(strip_order(world), vec![0, 1, 2]);
        // Laid out next to its neighbour again, not left where it was dropped.
        let moved = window_frame(world, 0);
        let neighbour = window_frame(world, 1);
        assert_eq!(moved.max.x, neighbour.min.x);
    });
}

#[test]
fn a_click_does_not_reorder() {
    run_gesture(
        TestHarness::new().with_windows(3),
        0,
        Gesture::Drag(5, 3),
        |world, _state| {
            assert_eq!(strip_order(world), vec![0, 1, 2]);
        },
    );
}

#[test]
fn resizing_by_an_edge_does_not_reorder() {
    run_gesture(
        TestHarness::new().with_windows(3),
        1,
        Gesture::ResizeLeft(300),
        |world, _state| {
            assert_eq!(strip_order(world), vec![0, 1, 2]);
        },
    );
}

#[test]
fn dropping_onto_another_display_moves_the_window_there() {
    let harness = TestHarness::new().with_windows(3).with_display(
        EXT_DISPLAY_ID,
        IRect::new(0, -EXT_DISPLAY_HEIGHT, EXT_DISPLAY_WIDTH, 0),
        vec![EXT_WORKSPACE_ID],
    );
    run_gesture(harness, 1, Gesture::Drag(100, -600), |world, _state| {
        assert_on_workspace!(world, 1, EXT_WORKSPACE_ID);
        assert_not_on_workspace!(world, 1, TEST_WORKSPACE_ID);
        assert_on_workspace!(world, 0, TEST_WORKSPACE_ID);
        assert_on_workspace!(world, 2, TEST_WORKSPACE_ID);
        let frame = window_frame(world, 1);
        assert!(
            frame.max.y <= 0,
            "window should sit on the upper display: {frame:?}"
        );
    });
}

#[test]
fn a_move_reported_after_the_release_still_reorders() {
    run_gesture(
        TestHarness::new().with_windows(3),
        0,
        Gesture::SilentDrag(560, 0),
        |world, _state| {
            assert_eq!(strip_order(world), vec![1, 0, 2]);
        },
    );
}

#[test]
fn a_window_not_carried_by_the_cursor_does_not_reorder() {
    run_gesture(
        TestHarness::new().with_windows(3),
        0,
        Gesture::Unrelated {
            window: Origin::new(560, 0),
            cursor: Origin::new(40, 200),
        },
        |world, _state| {
            assert_eq!(strip_order(world), vec![0, 1, 2]);
        },
    );
}

#[test]
fn dragging_a_floating_window_leaves_it_alone() {
    let mut params = WindowParams::new(".*", None);
    params.floating = Some(true);
    let config: Config = (MainOptions::default(), vec![params]).into();
    let harness = TestHarness::new().with_config(config).with_windows(2);
    run_gesture(harness, 0, Gesture::Drag(300, 50), |world, _state| {
        assert_eq!(strip_order(world), Vec::<WinID>::new());
        let entity = find_window_entity(0, world);
        assert!(world.entity(entity).contains::<Unmanaged>());
    });
}
