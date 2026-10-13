//! Scripted UI automation for development (`--features automation`).
//!
//! Set `GPUI_CUT_SCRIPT` to a file with one command per line:
//!
//! ```text
//! wait 500            # milliseconds
//! key cmd-j           # a keystroke, as in key bindings
//! type 1:23.5         # types text one character at a time
//! click 400 300       # left click at window coordinates (logical px)
//! dblclick 400 300
//! move 400 300
//! drag 100 700 300 700
//! press 100 700       # button down; then `dragto` and `release`
//! dragto 300 700      # moves with the button held, in small steps
//! release 300 700
//! mods shift alt      # modifiers for the following mouse events (`mods` clears)
//! scroll 400 700 -120 # vertical wheel delta in px at a point
//! shot /tmp/a.png     # saves the rendered window as PNG
//! quit
//! ```

use std::time::Duration;

use gpui_kit::{
    AnyWindowHandle, App, AsyncApp, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};

enum Command {
    Wait(u64),
    Key(String),
    Type(String),
    Click(f32, f32, usize),
    Move(f32, f32),
    Drag(f32, f32, f32, f32),
    Press(f32, f32),
    DragTo(f32, f32),
    Release(f32, f32),
    Mods(Modifiers),
    Scroll(f32, f32, f32),
    Shot(String),
    Quit,
}

fn parse(script: &str) -> Vec<Command> {
    script
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
            let nums: Vec<f32> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
            Some(match cmd {
                "wait" => Command::Wait(rest.trim().parse().ok()?),
                "key" => Command::Key(rest.trim().to_string()),
                "type" => Command::Type(rest.to_string()),
                "click" => Command::Click(nums[0], nums[1], 1),
                "dblclick" => Command::Click(nums[0], nums[1], 2),
                "move" => Command::Move(nums[0], nums[1]),
                "drag" => Command::Drag(nums[0], nums[1], nums[2], nums[3]),
                "press" => Command::Press(nums[0], nums[1]),
                "dragto" => Command::DragTo(nums[0], nums[1]),
                "release" => Command::Release(nums[0], nums[1]),
                "mods" => {
                    let mut m = Modifiers::default();
                    for name in rest.split_whitespace() {
                        match name {
                            "shift" => m.shift = true,
                            "alt" => m.alt = true,
                            "cmd" => m.platform = true,
                            "ctrl" => m.control = true,
                            other => eprintln!("automation: unknown modifier {other}"),
                        }
                    }
                    Command::Mods(m)
                }
                "scroll" => Command::Scroll(nums[0], nums[1], nums[2]),
                "shot" => Command::Shot(rest.trim().to_string()),
                "quit" => Command::Quit,
                other => {
                    eprintln!("automation: unknown command {other}");
                    return None;
                }
            })
        })
        .collect()
}

fn dispatch(handle: AnyWindowHandle, cx: &mut AsyncApp, event: PlatformInput) {
    let _ = handle.update(cx, |_, window, cx| {
        window.dispatch_event(event, cx);
        window.refresh();
    });
}

/// Starts the script from `GPUI_CUT_SCRIPT`, if set.
pub fn start(handle: AnyWindowHandle, cx: &mut App) {
    let Some(path) = std::env::var_os("GPUI_CUT_SCRIPT") else { return };
    let Ok(script) = std::fs::read_to_string(&path) else {
        eprintln!("automation: cannot read {}", path.to_string_lossy());
        return;
    };
    let commands = parse(&script);
    cx.spawn(async move |cx| {
        let mut mods = Modifiers::default();
        let mut pointer = point(px(0.), px(0.));
        for command in commands {
            match command {
                Command::Mods(m) => mods = m,
                Command::Press(x, y) => {
                    pointer = point(px(x), px(y));
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: pointer,
                            pressed_button: None,
                            modifiers: mods,
                        }),
                    );
                    cx.background_executor().timer(Duration::from_millis(30)).await;
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: pointer,
                            modifiers: mods,
                            click_count: 1,
                            first_mouse: false,
                        }),
                    );
                }
                Command::DragTo(x, y) => {
                    let from = pointer;
                    for i in 1..=10 {
                        let f = i as f32 / 10.0;
                        pointer = point(from.x + (px(x) - from.x) * f, from.y + (px(y) - from.y) * f);
                        cx.background_executor().timer(Duration::from_millis(30)).await;
                        dispatch(
                            handle,
                            cx,
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position: pointer,
                                pressed_button: Some(MouseButton::Left),
                                modifiers: mods,
                            }),
                        );
                    }
                }
                Command::Release(x, y) => dispatch(
                    handle,
                    cx,
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: point(px(x), px(y)),
                        modifiers: mods,
                        click_count: 1,
                    }),
                ),
                Command::Wait(ms) => cx.background_executor().timer(Duration::from_millis(ms)).await,
                Command::Key(k) => match Keystroke::parse(&k) {
                    Ok(ks) => {
                        let _ = handle.update(cx, |_, window, cx| {
                            window.dispatch_keystroke(ks, cx);
                        });
                    }
                    Err(e) => eprintln!("automation: bad keystroke {k}: {e}"),
                },
                Command::Type(text) => {
                    for ch in text.chars() {
                        let key = if ch == ' ' { "space".to_string() } else { ch.to_string() };
                        if let Ok(mut ks) = Keystroke::parse(&key) {
                            ks.key_char = Some(ch.to_string());
                            let _ = handle.update(cx, |_, window, cx| {
                                window.dispatch_keystroke(ks, cx);
                            });
                        }
                        cx.background_executor().timer(Duration::from_millis(15)).await;
                    }
                }
                Command::Move(x, y) => dispatch(
                    handle,
                    cx,
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position: point(px(x), px(y)),
                        pressed_button: None,
                        modifiers: mods,
                    }),
                ),
                Command::Click(x, y, count) => {
                    let position = point(px(x), px(y));
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseMove(MouseMoveEvent { position, pressed_button: None, modifiers: mods }),
                    );
                    cx.background_executor().timer(Duration::from_millis(30)).await;
                    for n in 1..=count {
                        dispatch(
                            handle,
                            cx,
                            PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers: mods,
                                click_count: n,
                                first_mouse: false,
                            }),
                        );
                        dispatch(
                            handle,
                            cx,
                            PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Left,
                                position,
                                modifiers: mods,
                                click_count: n,
                            }),
                        );
                    }
                }
                Command::Drag(x1, y1, x2, y2) => {
                    let from = point(px(x1), px(y1));
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: from,
                            pressed_button: None,
                            modifiers: mods,
                        }),
                    );
                    cx.background_executor().timer(Duration::from_millis(30)).await;
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: from,
                            modifiers: mods,
                            click_count: 1,
                            first_mouse: false,
                        }),
                    );
                    for i in 1..=10 {
                        let f = i as f32 / 10.0;
                        cx.background_executor().timer(Duration::from_millis(30)).await;
                        dispatch(
                            handle,
                            cx,
                            PlatformInput::MouseMove(MouseMoveEvent {
                                position: point(px(x1 + (x2 - x1) * f), px(y1 + (y2 - y1) * f)),
                                pressed_button: Some(MouseButton::Left),
                                modifiers: mods,
                            }),
                        );
                    }
                    dispatch(
                        handle,
                        cx,
                        PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: point(px(x2), px(y2)),
                            modifiers: mods,
                            click_count: 1,
                        }),
                    );
                }
                Command::Scroll(x, y, dy) => dispatch(
                    handle,
                    cx,
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(x), px(y)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
                        modifiers: mods,
                        touch_phase: TouchPhase::Moved,
                    }),
                ),
                Command::Shot(path) => {
                    let result = handle.update(cx, |_, window, _| window.render_to_image());
                    match result {
                        Ok(Ok(image)) => match image.save(&path) {
                            Ok(()) => eprintln!("automation: saved {path}"),
                            Err(e) => eprintln!("automation: could not save {path}: {e}"),
                        },
                        Ok(Err(e)) => eprintln!("automation: render failed: {e:#}"),
                        Err(e) => eprintln!("automation: window gone: {e:#}"),
                    }
                }
                Command::Quit => {
                    cx.update(|cx| cx.quit());
                    return;
                }
            }
        }
    })
    .detach();
}
