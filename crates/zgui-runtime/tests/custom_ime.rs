//! Custom canvas IME hosting (`data-zgui-ime="true"`) without `Editors` DOM text projection.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use zgui_dom::NodeKind;
use zgui_geom::{CssPx, DevicePx, Point, Size};
use zgui_platform::{SurfaceEvent, TextInputPurpose};
use zgui_reactive::RwSignal;
use zgui_reactive::prelude::{Get, GetUntracked, Set};
use zgui_view::{AttrName, BuildCx, IntoView, NodeRef, View, ViewHost};
use zgui_vocab::{
    ImeEvent, Key, KeyCode, KeyEvent, KeyState, Modifiers, NamedKey, PhysicalKey, PointerAction,
    PointerButton, PointerEvent, Timestamp, UiState,
};

const CSS: &str = "root { display: block; width: 400px; height: 300px }
                   .shell { display: block; padding-left: 30px; padding-top: 20px }
                   .terminal { display: block; width: 200px; height: 120px }
                   control { display: block; width: 100px; height: 24px }";

fn last_text_input(
    harness: &zgui_platform_headless::Harness<zgui_runtime::Runtime>,
) -> Option<Option<zgui_platform::TextInput>> {
    harness
        .platform()
        .offscreens()
        .first()
        .expect("a surface exists")
        .last_text_input()
}

fn count_text_children(
    harness: &zgui_platform_headless::Harness<zgui_runtime::Runtime>,
    host: NodeRef,
) -> usize {
    let window = &harness.app().windows()[0];
    let document = window.document().borrow();
    let store = document.store();
    let key = zgui_view_dom::id::to_document(host.get_untracked().expect("bound"))
        .expect("document node");
    let index = store.index_of(key).expect("live");
    let mut count = 0;
    let mut child = store.core(index).first_child();
    while let Some(node) = child {
        if store.core(node).kind() == NodeKind::Text {
            count += 1;
        }
        child = store.core(node).next_sibling();
    }
    count
}

#[test]
fn custom_ime_host_enables_surface_ime_tracks_cursor_attrs_and_creates_zero_text_nodes() {
    let host = NodeRef::new();
    let canvas = NodeRef::new();
    let events_seen: Rc<RefCell<Vec<String>>> = Rc::default();
    let seen = Rc::clone(&events_seen);

    let caret_x = RwSignal::new(Some("16".to_owned()));
    let caret_y = RwSignal::new(Some("32".to_owned()));
    let caret_h = RwSignal::new(Some("18".to_owned()));
    let disabled = RwSignal::new(false);

    let mut harness = support::app_with_text(CSS, move |cx: &mut BuildCx<'_>| {
        let seen_start = Rc::clone(&seen);
        let seen_preedit = Rc::clone(&seen);
        let seen_commit = Rc::clone(&seen);
        let seen_end = Rc::clone(&seen);
        let seen_key = Rc::clone(&seen);
        let seen_text = Rc::clone(&seen);
        Box::new(
            zgui_elements::column()
                .class("root")
                .child(
                    zgui_elements::r#box().class("shell").child(
                        zgui_elements::r#box()
                            .class("terminal")
                            .node_ref(host)
                            .state(UiState::DISABLED, move || disabled.get())
                            .attribute(AttrName::new("tabindex"), "0")
                            .attribute(AttrName::new("data-zgui-ime"), "true")
                            .attribute(AttrName::new("data-zgui-ime-x"), move || caret_x.get())
                            .attribute(AttrName::new("data-zgui-ime-y"), move || caret_y.get())
                            .attribute(AttrName::new("data-zgui-ime-h"), move || caret_h.get())
                            .on(zgui_view::events::IME_START, move |ev| {
                                seen_start.borrow_mut().push("ime_start".into());
                                ev.prevent_default();
                            })
                            .on(zgui_view::events::IME_PREEDIT, move |ev| {
                                if let ImeEvent::Preedit { text, .. } = &**ev {
                                    seen_preedit.borrow_mut().push(format!("preedit:{text}"));
                                }
                                ev.prevent_default();
                            })
                            .on(zgui_view::events::IME_COMMIT, move |ev| {
                                if let ImeEvent::Commit(text) = &**ev {
                                    seen_commit.borrow_mut().push(format!("commit:{text}"));
                                }
                                ev.prevent_default();
                            })
                            .on(zgui_view::events::IME_END, move |ev| {
                                seen_end.borrow_mut().push("ime_end".into());
                                ev.prevent_default();
                            })
                            .on(zgui_view::events::KEY_DOWN, move |ev| {
                                if let Key::Character(ch) = &ev.key {
                                    seen_key.borrow_mut().push(format!("key:{ch}"));
                                    ev.prevent_default();
                                }
                            })
                            .on(zgui_view::events::TEXT, move |ev| {
                                seen_text.borrow_mut().push(format!("text:{}", ev.text));
                                ev.prevent_default();
                            })
                            .child(zgui_elements::r#box().node_ref(canvas)),
                    ),
                )
                .child(zgui_elements::control())
                .into_view()
                .build(cx),
        )
    });
    harness.settle(8);

    // Click inside the custom IME host (at (50, 40), inside shell (30, 20) + terminal 200x120).
    let mut mouse = PointerEvent::mouse(Point::new(CssPx(50.0), CssPx(40.0)));
    mouse.button = Some(PointerButton::Primary);
    harness.deliver_to_first(SurfaceEvent::Pointer {
        action: PointerAction::Pressed,
        event: mouse,
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    harness.deliver_to_first(SurfaceEvent::Pointer {
        action: PointerAction::Released,
        event: mouse,
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    harness.settle(8);

    let focused = harness.app().windows()[0].host().focused().get_untracked();
    assert_eq!(focused, host.get_untracked());
    assert_eq!(count_text_children(&harness, host), 0);

    // Border box sits at (30, 20); cursor cell (16, 32, h=18) -> (46, 52) size (1, 18).
    let initial = last_text_input(&harness)
        .expect("surface told")
        .expect("IME enabled");
    assert_eq!(initial.caret_origin, Point::new(CssPx(46.0), CssPx(52.0)));
    assert_eq!(initial.caret_size, Size::new(CssPx(1.0), CssPx(18.0)));
    assert_eq!(initial.purpose, TextInputPurpose::Normal);

    // Updating cursor attributes dynamically updates surface text input area on the next frame,
    // and out-of-bounds values clamp cleanly inside the 200x120 border box.
    caret_x.set(Some("250".to_owned()));
    caret_y.set(Some("115".to_owned()));
    caret_h.set(Some("20".to_owned()));
    harness.settle(8);

    let clamped = last_text_input(&harness)
        .expect("surface told")
        .expect("IME still enabled");
    assert_eq!(
        clamped.caret_origin,
        Point::new(CssPx(230.0), CssPx(120.0)),
        "x clamped to 200 (30+200) and y clamped to 120-20=100 (20+100)"
    );
    assert_eq!(clamped.caret_size, Size::new(CssPx(1.0), CssPx(20.0)));

    // Clearing optional attributes falls back to (0, 0) and border_box.size.height (120).
    caret_x.set(None);
    caret_y.set(None);
    caret_h.set(None);
    harness.settle(8);
    let fallback = last_text_input(&harness)
        .expect("surface told")
        .expect("IME enabled with fallback");
    assert_eq!(fallback.caret_origin, Point::new(CssPx(30.0), CssPx(20.0)));
    assert_eq!(fallback.caret_size, Size::new(CssPx(1.0), CssPx(120.0)));

    // Deliver IME lifecycle + KeyDown; verify Tab during active preedit does NOT steal focus,
    // all events reach the custom host, and zero `<text>` DOM nodes are ever created.
    harness.deliver_to_first(SurfaceEvent::Ime(ImeEvent::Enabled));
    harness.deliver_to_first(SurfaceEvent::Ime(ImeEvent::Preedit {
        text: "かん".into(),
        cursor: Some(6..6),
    }));
    harness.deliver_to_first(SurfaceEvent::Key {
        state: KeyState::Pressed,
        event: KeyEvent::named(NamedKey::Tab, PhysicalKey::Code(KeyCode::Tab)),
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    harness.settle(8);
    let focused_during_ime = harness.app().windows()[0].host().focused().get_untracked();
    assert_eq!(
        focused_during_ime,
        host.get_untracked(),
        "Tab during composition must not move focus away from custom IME host"
    );

    harness.deliver_to_first(SurfaceEvent::Ime(ImeEvent::Commit("漢".into())));
    harness.deliver_to_first(SurfaceEvent::Ime(ImeEvent::Disabled));
    harness.deliver_to_first(SurfaceEvent::Key {
        state: KeyState::Pressed,
        event: KeyEvent {
            key: Key::Character("ls".into()),
            key_without_modifiers: Key::Character("ls".into()),
            physical: PhysicalKey::Code(KeyCode::KeyL),
            location: zgui_vocab::KeyLocation::Standard,
            repeat: false,
        },
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    harness.settle(8);

    assert_eq!(
        *events_seen.borrow(),
        vec![
            "ime_start",
            "preedit:かん",
            "commit:漢",
            "ime_end",
            "key:ls",
        ]
    );
    assert_eq!(
        count_text_children(&harness, host),
        0,
        "Editors must never attach or append <text> nodes under a data-zgui-ime host"
    );

    // Disabling the host turns surface IME off.
    disabled.set(true);
    harness.settle(8);
    assert_eq!(last_text_input(&harness), Some(None));
    harness.shut_down();
}

#[test]
fn custom_ime_host_focused_before_layout_scales_correctly_and_disables_on_blur() {
    let host = NodeRef::new();
    let mut harness = support::app_with_text(CSS, move |cx: &mut BuildCx<'_>| {
        let view = zgui_elements::column()
            .class("root")
            .child(
                zgui_elements::r#box().class("shell").child(
                    zgui_elements::r#box()
                        .class("terminal")
                        .node_ref(host)
                        .attribute(AttrName::new("tabindex"), "0")
                        .attribute(AttrName::new("data-zgui-ime"), "true")
                        .attribute(AttrName::new("data-zgui-ime-x"), "12.5")
                        .attribute(AttrName::new("data-zgui-ime-y"), "24")
                        .attribute(AttrName::new("data-zgui-ime-h"), "16"),
                ),
            )
            .child(zgui_elements::control())
            .into_view()
            .build(cx);
        host.focus();
        Box::new(view)
    });
    harness.settle(8);

    // Switch to 2x HiDPI scale and ensure CSS-pixel cursor area stays exact in CSS pixels.
    harness.deliver_to_first(SurfaceEvent::ScaleFactorChanged {
        scale_factor: 2.0,
        size: Size::new(DevicePx(800.0), DevicePx(600.0)),
    });
    harness.settle(8);

    let area = last_text_input(&harness)
        .expect("surface told")
        .expect("IME enabled after first layout and scale change");
    assert_eq!(area.caret_origin, Point::new(CssPx(42.5), CssPx(44.0)));
    assert_eq!(area.caret_size, Size::new(CssPx(1.0), CssPx(16.0)));
    assert_eq!(count_text_children(&harness, host), 0);

    // Tabbing away to `<control>` disables surface IME.
    harness.deliver_to_first(SurfaceEvent::Key {
        state: KeyState::Pressed,
        event: KeyEvent::named(NamedKey::Tab, PhysicalKey::Code(KeyCode::Tab)),
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    harness.settle(8);
    assert_eq!(last_text_input(&harness), Some(None));
    harness.shut_down();
}
