// Native Double-Buffered Desktop GUI Toolkit (`gui_app`) for Kovrek.
// Uses Win32 `user32.dll`, `gdi32.dll`, `dwmapi.dll`, and `kernel32.dll`
// dynamically via `libloading` on Windows so desktop `.kvr` and `--gui` bundled
// executables open in their own crisp dark/light native OS window without a
// console window behind them.

#![allow(clippy::too_many_arguments)]
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
#[cfg(windows)]
use std::collections::VecDeque;

use crate::value::Val;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WidgetKind {
    Label { style: String, align: TextAlign },
    Display { align: TextAlign },
    Button { role: String, flex: f32 },
    List { items: Vec<String>, scroll: usize },
    Progress { value: f64 },
}

#[derive(Clone, Debug)]
pub(crate) struct WidgetNode {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) height: i32,
    pub(crate) kind: WidgetKind,
}

#[derive(Clone, Debug)]
pub(crate) enum LayoutEntry {
    Single(WidgetNode),
    Row { height: i32, items: Vec<WidgetNode> },
}

#[derive(Clone, Debug)]
pub(crate) struct HitRect {
    pub(crate) id: String,
    pub(crate) is_list: bool,
    pub(crate) scroll: usize,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct GuiLayout {
    pub(crate) title: String,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) theme: ThemeMode,
    pub(crate) entries: Vec<LayoutEntry>,
}

#[derive(Clone, Debug)]
pub(crate) enum UiEvent {
    Click {
        id: String,
    },
    ListClick {
        id: String,
        index: usize,
        text: String,
    },
    Char {
        ch: char,
    },
    Key {
        key: String,
        ctrl: bool,
    },
    Paste {
        text: String,
    },
}

fn map_str(pairs: &[(Val, Val)], key: &str) -> Option<String> {
    pairs.iter().find_map(|(k, v)| {
        if matches!(k, Val::Str(s) if s == key) {
            Some(v.show())
        } else {
            None
        }
    })
}

fn map_val<'a>(pairs: &'a [(Val, Val)], key: &str) -> Option<&'a Val> {
    pairs.iter().find_map(|(k, v)| {
        if matches!(k, Val::Str(s) if s == key) {
            Some(v)
        } else {
            None
        }
    })
}

fn map_i32(pairs: &[(Val, Val)], key: &str, default: i32) -> i32 {
    match map_val(pairs, key) {
        Some(Val::Num(n)) => *n as i32,
        _ => default,
    }
}

fn map_f32(pairs: &[(Val, Val)], key: &str, default: f32) -> f32 {
    match map_val(pairs, key) {
        Some(Val::Num(n)) => *n as f32,
        _ => default,
    }
}

fn parse_align(raw: Option<String>, default: TextAlign) -> TextAlign {
    match raw.as_deref() {
        Some("left") => TextAlign::Left,
        Some("center") => TextAlign::Center,
        Some("right") => TextAlign::Right,
        _ => default,
    }
}

fn parse_widget(pairs: &[(Val, Val)], fallback_idx: usize) -> Result<WidgetNode, String> {
    let wtype = map_str(pairs, "type").unwrap_or_else(|| "label".into());
    let id = map_str(pairs, "id").unwrap_or_else(|| format!("__w_{fallback_idx}"));
    let text = map_str(pairs, "text").unwrap_or_default();
    let default_h = match wtype.as_str() {
        "display" => 62,
        "list" => 96,
        "button" => 44,
        "progress" => 26,
        _ => 26,
    };
    let height = map_i32(pairs, "height", default_h).max(16);
    let kind = match wtype.as_str() {
        "label" => WidgetKind::Label {
            style: map_str(pairs, "style").unwrap_or_else(|| "normal".into()),
            align: parse_align(map_str(pairs, "align"), TextAlign::Left),
        },
        "display" => WidgetKind::Display {
            align: parse_align(map_str(pairs, "align"), TextAlign::Right),
        },
        "button" => WidgetKind::Button {
            role: map_str(pairs, "role").unwrap_or_else(|| "default".into()),
            flex: map_f32(pairs, "flex", 1.0).max(0.2),
        },
        "list" => {
            let items = match map_val(pairs, "items") {
                Some(Val::List(v)) => v.iter().map(Val::show).collect(),
                _ => Vec::new(),
            };
            WidgetKind::List { items, scroll: 0 }
        }
        "progress" => {
            let value = match map_val(pairs, "value") {
                Some(Val::Num(n)) => n.clamp(0.0, 1.0),
                _ => 0.0,
            };
            WidgetKind::Progress { value }
        }
        other => return Err(format!("gui_app: unsupported widget type '{other}'")),
    };
    Ok(WidgetNode {
        id,
        text,
        height,
        kind,
    })
}

pub(crate) fn parse_gui_layout(spec: &Val) -> Result<GuiLayout, String> {
    let Val::Map(root) = spec else {
        return Err("gui_app expects (layout_map, on_event_fn)".into());
    };
    let title = map_str(root, "title").unwrap_or_else(|| "Kovrek App".into());
    let width = map_i32(root, "width", 460).clamp(240, 1920);
    let height = map_i32(root, "height", 640).clamp(180, 1200);
    let theme = match map_str(root, "theme").as_deref() {
        Some("light") => ThemeMode::Light,
        _ => ThemeMode::Dark,
    };
    let mut entries = Vec::new();
    let Some(Val::List(rows)) = map_val(root, "rows") else {
        return Err("gui_app layout requires a 'rows' List".into());
    };
    for (idx, row_val) in rows.iter().enumerate() {
        let Val::Map(rpairs) = row_val else {
            return Err("gui_app row entry must be a Map".into());
        };
        if map_str(rpairs, "type").as_deref() == Some("row") {
            let row_h = map_i32(rpairs, "height", 46).max(24);
            let mut items = Vec::new();
            if let Some(Val::List(row_items)) = map_val(rpairs, "items") {
                for (cidx, iv) in row_items.iter().enumerate() {
                    let Val::Map(ipairs) = iv else {
                        return Err("gui_app row item must be a Map".into());
                    };
                    items.push(parse_widget(ipairs, idx * 100 + cidx)?);
                }
            }
            entries.push(LayoutEntry::Row {
                height: row_h,
                items,
            });
        } else {
            entries.push(LayoutEntry::Single(parse_widget(rpairs, idx)?));
        }
    }
    Ok(GuiLayout {
        title,
        width,
        height,
        theme,
        entries,
    })
}

/// Computes clickable screen geometry rectangles from the layout and current client area.
pub(crate) fn compute_hit_rects(
    layout: &GuiLayout,
    client_w: i32,
    client_h: i32,
) -> Vec<(HitRect, WidgetNode)> {
    let pad = 16;
    let gap = 8;
    let inner_w = (client_w - pad * 2).max(80);
    let mut fixed_h = pad * 2 + (layout.entries.len().saturating_sub(1) as i32) * gap;
    let mut flex_rows = 0i32;
    for entry in &layout.entries {
        match entry {
            LayoutEntry::Single(w) => fixed_h += w.height,
            LayoutEntry::Row { height, .. } => {
                fixed_h += *height;
                flex_rows += 1;
            }
        }
    }
    let extra_h = (client_h - fixed_h).max(0);
    let per_row_extra = if flex_rows > 0 {
        extra_h / flex_rows
    } else {
        0
    };

    let mut out = Vec::new();
    let mut y = pad;
    for entry in &layout.entries {
        match entry {
            LayoutEntry::Single(w) => {
                let (is_list, scroll) = match &w.kind {
                    WidgetKind::List { scroll, .. } => (true, *scroll),
                    _ => (false, 0),
                };
                out.push((
                    HitRect {
                        id: w.id.clone(),
                        is_list,
                        scroll,
                        x: pad,
                        y,
                        w: inner_w,
                        h: w.height,
                    },
                    w.clone(),
                ));
                y += w.height + gap;
            }
            LayoutEntry::Row { height, items } => {
                let row_h = *height + per_row_extra;
                if !items.is_empty() {
                    let total_flex: f32 = items
                        .iter()
                        .map(|w| match w.kind {
                            WidgetKind::Button { flex, .. } => flex,
                            _ => 1.0,
                        })
                        .sum::<f32>()
                        .max(1.0);
                    let total_gap = gap * (items.len().saturating_sub(1) as i32);
                    let avail_w = (inner_w - total_gap).max(40) as f32;
                    let mut x = pad;
                    for (i, w) in items.iter().enumerate() {
                        let weight = match w.kind {
                            WidgetKind::Button { flex, .. } => flex,
                            _ => 1.0,
                        };
                        let item_w = if i + 1 == items.len() {
                            (pad + inner_w - x).max(20)
                        } else {
                            ((avail_w * (weight / total_flex)).round() as i32).max(20)
                        };
                        out.push((
                            HitRect {
                                id: w.id.clone(),
                                is_list: false,
                                scroll: 0,
                                x,
                                y,
                                w: item_w,
                                h: row_h,
                            },
                            w.clone(),
                        ));
                        x += item_w + gap;
                    }
                }
                y += row_h + gap;
            }
        }
    }
    out
}

fn apply_widget_patch(w: &mut WidgetNode, updates: &HashMap<String, Val>) {
    if let Some(val) = updates.get(&w.id) {
        match &mut w.kind {
            WidgetKind::List { items, scroll } => {
                if let Val::List(vs) = val {
                    *items = vs.iter().map(Val::show).collect();
                    *scroll = items.len().saturating_sub(4);
                }
            }
            WidgetKind::Progress { value } => {
                if let Val::Num(n) = val {
                    *value = n.clamp(0.0, 1.0);
                }
            }
            _ => {
                w.text = val.show();
            }
        }
    }
    let role_key = format!("{}.role", w.id);
    if let Some(Val::Str(r)) = updates.get(&role_key)
        && let WidgetKind::Button { role, .. } = &mut w.kind
    {
        *role = r.clone();
    }
}

/// Applies updates returned from a Kovrek `on_event` callback onto the layout;
/// returns `true` when the callback requested closing the window (`"close": true`).
pub(crate) fn apply_layout_updates(layout: &mut GuiLayout, patch: &Val) -> (bool, bool) {
    let Val::Map(pairs) = patch else {
        return (false, false);
    };
    let mut map = HashMap::new();
    let mut close_req = false;
    let mut new_title: Option<String> = None;
    for (k, v) in pairs {
        if let Val::Str(name) = k {
            if name == "close" && matches!(v, Val::Bool(true)) {
                close_req = true;
            } else if name == "title" {
                new_title = Some(v.show());
            } else {
                map.insert(name.clone(), v.clone());
            }
        }
    }
    if let Some(t) = new_title {
        layout.title = t;
    }
    for entry in &mut layout.entries {
        match entry {
            LayoutEntry::Single(w) => apply_widget_patch(w, &map),
            LayoutEntry::Row { items, .. } => {
                for w in items {
                    apply_widget_patch(w, &map);
                }
            }
        }
    }
    (close_req, !map.is_empty())
}

fn ui_event_to_val(ev: &UiEvent) -> Val {
    let mut pairs = Vec::new();
    match ev {
        UiEvent::Click { id } => {
            pairs.push((Val::Str("kind".into()), Val::Str("click".into())));
            pairs.push((Val::Str("id".into()), Val::Str(id.clone())));
        }
        UiEvent::ListClick { id, index, text } => {
            pairs.push((Val::Str("kind".into()), Val::Str("list_click".into())));
            pairs.push((Val::Str("id".into()), Val::Str(id.clone())));
            pairs.push((Val::Str("index".into()), Val::Num(*index as f64)));
            pairs.push((Val::Str("text".into()), Val::Str(text.clone())));
        }
        UiEvent::Char { ch } => {
            pairs.push((Val::Str("kind".into()), Val::Str("char".into())));
            pairs.push((Val::Str("char".into()), Val::Str(ch.to_string())));
        }
        UiEvent::Key { key, ctrl } => {
            pairs.push((Val::Str("kind".into()), Val::Str("key".into())));
            pairs.push((Val::Str("key".into()), Val::Str(key.clone())));
            pairs.push((Val::Str("ctrl".into()), Val::Bool(*ctrl)));
        }
        UiEvent::Paste { text } => {
            pairs.push((Val::Str("kind".into()), Val::Str("paste".into())));
            pairs.push((Val::Str("text".into()), Val::Str(text.clone())));
        }
    }
    Val::Map(pairs)
}

#[cfg(windows)]
mod win32_backend {
    use super::*;
    use core::ffi::c_void;
    use libloading::Library;
    use std::cell::RefCell;
    use std::rc::Rc;

    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;
    const CS_HREDRAW: u32 = 0x0002;
    const CS_VREDRAW: u32 = 0x0001;
    const GWLP_USERDATA: i32 = -21;
    const SW_SHOW: i32 = 5;
    const PM_REMOVE: u32 = 0x0001;
    const IDC_ARROW: usize = 32512;
    const IDC_HAND: usize = 32649;

    const WM_DESTROY: u32 = 0x0002;
    const WM_SIZE: u32 = 0x0005;
    const WM_PAINT: u32 = 0x000F;
    const WM_CLOSE: u32 = 0x0010;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_KEYDOWN: u32 = 0x0100;
    const WM_CHAR: u32 = 0x0102;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const WM_MOUSELEAVE: u32 = 0x02A3;
    const TME_LEAVE: u32 = 0x0000_0002;

    const SRCCOPY: u32 = 0x00CC_0020;
    const TRANSPARENT: i32 = 1;
    const PS_SOLID: i32 = 0;
    const DT_LEFT: u32 = 0x0000;
    const DT_CENTER: u32 = 0x0001;
    const DT_RIGHT: u32 = 0x0002;
    const DT_VCENTER: u32 = 0x0004;
    const DT_SINGLELINE: u32 = 0x0020;
    const DT_END_ELLIPSIS: u32 = 0x8000;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    const fn colorref(r: u8, g: u8, b: u8) -> u32 {
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Msg {
        hwnd: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: Point,
    }

    #[repr(C)]
    struct PaintStruct {
        hdc: isize,
        f_erase: i32,
        rc_paint: Rect,
        f_restore: i32,
        f_inc_update: i32,
        rgb_reserved: [u8; 32],
    }

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        lpfn_wnd_proc: WndProcFn,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: isize,
        h_icon: isize,
        h_cursor: isize,
        hbr_background: isize,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
        h_icon_sm: isize,
    }

    #[repr(C)]
    struct TrackMouseEventStruct {
        cb_size: u32,
        dw_flags: u32,
        hwnd_track: isize,
        dw_hover_time: u32,
    }

    type WndProcFn = unsafe extern "system" fn(isize, u32, usize, isize) -> isize;

    struct WinApi {
        _kernel32: Library,
        _user32: Library,
        _gdi32: Library,
        def_window_proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
        register_class_ex: unsafe extern "system" fn(*const WndClassExW) -> u16,
        create_window_ex: unsafe extern "system" fn(
            u32,
            *const u16,
            *const u16,
            u32,
            i32,
            i32,
            i32,
            i32,
            isize,
            isize,
            isize,
            *const c_void,
        ) -> isize,
        show_window: unsafe extern "system" fn(isize, i32) -> i32,
        update_window: unsafe extern "system" fn(isize) -> i32,
        destroy_window: unsafe extern "system" fn(isize) -> i32,
        peek_message: unsafe extern "system" fn(*mut Msg, isize, u32, u32, u32) -> i32,
        wait_message: unsafe extern "system" fn() -> i32,
        translate_message: unsafe extern "system" fn(*const Msg) -> i32,
        dispatch_message: unsafe extern "system" fn(*const Msg) -> isize,
        post_quit_message: unsafe extern "system" fn(i32),
        begin_paint: unsafe extern "system" fn(isize, *mut PaintStruct) -> isize,
        end_paint: unsafe extern "system" fn(isize, *const PaintStruct) -> i32,
        invalidate_rect: unsafe extern "system" fn(isize, *const Rect, i32) -> i32,
        get_client_rect: unsafe extern "system" fn(isize, *mut Rect) -> i32,
        adjust_window_rect_ex: unsafe extern "system" fn(*mut Rect, u32, i32, u32) -> i32,
        set_window_text: unsafe extern "system" fn(isize, *const u16) -> i32,
        set_window_long_ptr: unsafe extern "system" fn(isize, i32, isize) -> isize,
        load_cursor: unsafe extern "system" fn(isize, usize) -> isize,
        set_cursor: unsafe extern "system" fn(isize) -> isize,
        track_mouse_event: unsafe extern "system" fn(*mut TrackMouseEventStruct) -> i32,
        get_key_state: unsafe extern "system" fn(i32) -> i16,
        fill_rect: unsafe extern "system" fn(isize, *const Rect, isize) -> i32,
        draw_text: unsafe extern "system" fn(isize, *const u16, i32, *mut Rect, u32) -> i32,
        open_clipboard: unsafe extern "system" fn(isize) -> i32,
        close_clipboard: unsafe extern "system" fn() -> i32,
        empty_clipboard: unsafe extern "system" fn() -> i32,
        get_clipboard_data: unsafe extern "system" fn(u32) -> isize,
        set_clipboard_data: unsafe extern "system" fn(u32, isize) -> isize,
        global_alloc: unsafe extern "system" fn(u32, usize) -> isize,
        global_lock: unsafe extern "system" fn(isize) -> *mut c_void,
        global_unlock: unsafe extern "system" fn(isize) -> i32,
        create_compatible_dc: unsafe extern "system" fn(isize) -> isize,
        create_compatible_bitmap: unsafe extern "system" fn(isize, i32, i32) -> isize,
        select_object: unsafe extern "system" fn(isize, isize) -> isize,
        delete_object: unsafe extern "system" fn(isize) -> i32,
        delete_dc: unsafe extern "system" fn(isize) -> i32,
        bit_blt: unsafe extern "system" fn(isize, i32, i32, i32, i32, isize, i32, i32, u32) -> i32,
        create_solid_brush: unsafe extern "system" fn(u32) -> isize,
        create_pen: unsafe extern "system" fn(i32, i32, u32) -> isize,
        round_rect: unsafe extern "system" fn(isize, i32, i32, i32, i32, i32, i32) -> i32,
        set_bk_mode: unsafe extern "system" fn(isize, i32) -> i32,
        set_text_color: unsafe extern "system" fn(isize, u32) -> u32,
        create_font: unsafe extern "system" fn(
            i32,
            i32,
            i32,
            i32,
            i32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            u32,
            *const u16,
        ) -> isize,
    }

    impl WinApi {
        unsafe fn load() -> Result<Self, String> {
            let kernel32 = unsafe { Library::new("kernel32.dll") }.map_err(|e| e.to_string())?;
            let user32 = unsafe { Library::new("user32.dll") }.map_err(|e| e.to_string())?;
            let gdi32 = unsafe { Library::new("gdi32.dll") }.map_err(|e| e.to_string())?;

            macro_rules! sym {
                ($lib:expr, $name:literal, $ty:ty) => {{
                    let s: libloading::Symbol<$ty> =
                        unsafe { $lib.get($name) }.map_err(|e| e.to_string())?;
                    *s
                }};
            }

            if let Ok(dpi) = unsafe {
                user32.get::<unsafe extern "system" fn(isize) -> i32>(
                    b"SetProcessDpiAwarenessContext\0",
                )
            } {
                unsafe { dpi(-4isize) };
            }

            Ok(Self {
                def_window_proc: sym!(user32, b"DefWindowProcW\0", _),
                register_class_ex: sym!(user32, b"RegisterClassExW\0", _),
                create_window_ex: sym!(user32, b"CreateWindowExW\0", _),
                show_window: sym!(user32, b"ShowWindow\0", _),
                update_window: sym!(user32, b"UpdateWindow\0", _),
                destroy_window: sym!(user32, b"DestroyWindow\0", _),
                peek_message: sym!(user32, b"PeekMessageW\0", _),
                wait_message: sym!(user32, b"WaitMessage\0", _),
                translate_message: sym!(user32, b"TranslateMessage\0", _),
                dispatch_message: sym!(user32, b"DispatchMessageW\0", _),
                post_quit_message: sym!(user32, b"PostQuitMessage\0", _),
                begin_paint: sym!(user32, b"BeginPaint\0", _),
                end_paint: sym!(user32, b"EndPaint\0", _),
                invalidate_rect: sym!(user32, b"InvalidateRect\0", _),
                get_client_rect: sym!(user32, b"GetClientRect\0", _),
                adjust_window_rect_ex: sym!(user32, b"AdjustWindowRectEx\0", _),
                set_window_text: sym!(user32, b"SetWindowTextW\0", _),
                set_window_long_ptr: sym!(user32, b"SetWindowLongPtrW\0", _),
                load_cursor: sym!(user32, b"LoadCursorW\0", _),
                set_cursor: sym!(user32, b"SetCursor\0", _),
                track_mouse_event: sym!(user32, b"TrackMouseEvent\0", _),
                get_key_state: sym!(user32, b"GetKeyState\0", _),
                fill_rect: sym!(user32, b"FillRect\0", _),
                draw_text: sym!(user32, b"DrawTextW\0", _),
                open_clipboard: sym!(user32, b"OpenClipboard\0", _),
                close_clipboard: sym!(user32, b"CloseClipboard\0", _),
                empty_clipboard: sym!(user32, b"EmptyClipboard\0", _),
                get_clipboard_data: sym!(user32, b"GetClipboardData\0", _),
                set_clipboard_data: sym!(user32, b"SetClipboardData\0", _),
                global_alloc: sym!(kernel32, b"GlobalAlloc\0", _),
                global_lock: sym!(kernel32, b"GlobalLock\0", _),
                global_unlock: sym!(kernel32, b"GlobalUnlock\0", _),
                create_compatible_dc: sym!(gdi32, b"CreateCompatibleDC\0", _),
                create_compatible_bitmap: sym!(gdi32, b"CreateCompatibleBitmap\0", _),
                select_object: sym!(gdi32, b"SelectObject\0", _),
                delete_object: sym!(gdi32, b"DeleteObject\0", _),
                delete_dc: sym!(gdi32, b"DeleteDC\0", _),
                bit_blt: sym!(gdi32, b"BitBlt\0", _),
                create_solid_brush: sym!(gdi32, b"CreateSolidBrush\0", _),
                create_pen: sym!(gdi32, b"CreatePen\0", _),
                round_rect: sym!(gdi32, b"RoundRect\0", _),
                set_bk_mode: sym!(gdi32, b"SetBkMode\0", _),
                set_text_color: sym!(gdi32, b"SetTextColor\0", _),
                create_font: sym!(gdi32, b"CreateFontW\0", _),
                _kernel32: kernel32,
                _user32: user32,
                _gdi32: gdi32,
            })
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn detach_explorer_console() {
        if let Ok(k32) = unsafe { Library::new("kernel32.dll") }
            && let (Ok(get_list), Ok(free_con)) = (
                unsafe {
                    k32.get::<unsafe extern "system" fn(*mut u32, u32) -> u32>(
                        b"GetConsoleProcessList\0",
                    )
                },
                unsafe { k32.get::<unsafe extern "system" fn() -> i32>(b"FreeConsole\0") },
            )
        {
            let mut pids = [0u32; 4];
            let count = unsafe { get_list(pids.as_mut_ptr(), 4) };
            // Explorer double-click allocates a fresh single-owner console; detach so only the GUI shows.
            if count == 1 {
                unsafe { free_con() };
            }
        }
    }

    fn enable_dark_frame(hwnd: isize, dark: bool) {
        if let Ok(dwm) = unsafe { Library::new("dwmapi.dll") }
            && let Ok(set_attr) = unsafe {
                dwm.get::<unsafe extern "system" fn(isize, u32, *const u32, u32) -> i32>(
                    b"DwmSetWindowAttribute\0",
                )
            }
        {
            let val: u32 = u32::from(dark);
            let _ = unsafe { set_attr(hwnd, 20, &val, 4) };
            let corner_round: u32 = 2;
            let _ = unsafe { set_attr(hwnd, 33, &corner_round, 4) };
        }
    }

    struct SharedWindowState {
        api: WinApi,
        layout: GuiLayout,
        hits: Vec<(HitRect, WidgetNode)>,
        hover_id: Option<String>,
        pressed_id: Option<String>,
        tracking_mouse: bool,
        closed: bool,
        events: VecDeque<UiEvent>,
    }

    struct Palette {
        bg: u32,
        card: u32,
        card_border: u32,
        text_primary: u32,
        text_muted: u32,
        text_accent: u32,
        text_error: u32,
        btn_default: u32,
        btn_default_hover: u32,
        btn_num: u32,
        btn_num_hover: u32,
        btn_op: u32,
        btn_op_hover: u32,
        btn_fn: u32,
        btn_fn_hover: u32,
        btn_accent: u32,
        btn_accent_hover: u32,
        btn_danger: u32,
        btn_danger_hover: u32,
        btn_border: u32,
        prog_track: u32,
        prog_fill: u32,
    }

    fn palette(mode: ThemeMode) -> Palette {
        match mode {
            ThemeMode::Dark => Palette {
                bg: colorref(17, 20, 29),
                card: colorref(25, 30, 44),
                card_border: colorref(46, 55, 79),
                text_primary: colorref(241, 245, 253),
                text_muted: colorref(138, 150, 178),
                text_accent: colorref(120, 186, 255),
                text_error: colorref(255, 118, 138),
                btn_default: colorref(33, 40, 58),
                btn_default_hover: colorref(45, 55, 78),
                btn_num: colorref(36, 43, 62),
                btn_num_hover: colorref(49, 59, 85),
                btn_op: colorref(41, 51, 77),
                btn_op_hover: colorref(56, 70, 105),
                btn_fn: colorref(31, 36, 54),
                btn_fn_hover: colorref(44, 52, 77),
                btn_accent: colorref(74, 122, 255),
                btn_accent_hover: colorref(96, 141, 255),
                btn_danger: colorref(64, 34, 46),
                btn_danger_hover: colorref(86, 45, 61),
                btn_border: colorref(54, 65, 92),
                prog_track: colorref(34, 40, 58),
                prog_fill: colorref(74, 122, 255),
            },
            ThemeMode::Light => Palette {
                bg: colorref(243, 245, 249),
                card: colorref(255, 255, 255),
                card_border: colorref(213, 220, 232),
                text_primary: colorref(24, 30, 44),
                text_muted: colorref(102, 112, 133),
                text_accent: colorref(37, 99, 235),
                text_error: colorref(217, 45, 32),
                btn_default: colorref(255, 255, 255),
                btn_default_hover: colorref(238, 242, 248),
                btn_num: colorref(255, 255, 255),
                btn_num_hover: colorref(236, 241, 249),
                btn_op: colorref(228, 236, 252),
                btn_op_hover: colorref(211, 224, 249),
                btn_fn: colorref(235, 238, 246),
                btn_fn_hover: colorref(221, 226, 238),
                btn_accent: colorref(37, 99, 235),
                btn_accent_hover: colorref(29, 78, 216),
                btn_danger: colorref(254, 228, 226),
                btn_danger_hover: colorref(254, 205, 202),
                btn_border: colorref(208, 215, 228),
                prog_track: colorref(223, 228, 238),
                prog_fill: colorref(37, 99, 235),
            },
        }
    }

    unsafe fn paint_window(hwnd: isize, st: &mut SharedWindowState) {
        let mut ps = unsafe { std::mem::zeroed::<PaintStruct>() };
        let hdc = unsafe { (st.api.begin_paint)(hwnd, &mut ps) };
        if hdc == 0 {
            return;
        }
        let mut cr = Rect::default();
        unsafe { (st.api.get_client_rect)(hwnd, &mut cr) };
        let cw = (cr.right - cr.left).max(1);
        let ch = (cr.bottom - cr.top).max(1);
        st.hits = compute_hit_rects(&st.layout, cw, ch);

        let mem_dc = unsafe { (st.api.create_compatible_dc)(hdc) };
        let mem_bmp = unsafe { (st.api.create_compatible_bitmap)(hdc, cw, ch) };
        let old_bmp = unsafe { (st.api.select_object)(mem_dc, mem_bmp) };
        unsafe { (st.api.set_bk_mode)(mem_dc, TRANSPARENT) };

        let pal = palette(st.layout.theme);
        let bg_brush = unsafe { (st.api.create_solid_brush)(pal.bg) };
        let full = Rect {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        unsafe { (st.api.fill_rect)(mem_dc, &full, bg_brush) };
        unsafe { (st.api.delete_object)(bg_brush) };

        let ui_font = wide("Segoe UI");
        let mono_font = wide("Consolas");
        let f_label = unsafe {
            (st.api.create_font)(-15, 0, 0, 0, 500, 0, 0, 0, 1, 0, 0, 5, 0, ui_font.as_ptr())
        };
        let f_title = unsafe {
            (st.api.create_font)(-18, 0, 0, 0, 650, 0, 0, 0, 1, 0, 0, 5, 0, ui_font.as_ptr())
        };
        let f_disp = unsafe {
            (st.api.create_font)(
                -34,
                0,
                0,
                0,
                650,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                mono_font.as_ptr(),
            )
        };
        let f_btn = unsafe {
            (st.api.create_font)(-17, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, ui_font.as_ptr())
        };
        let f_list = unsafe {
            (st.api.create_font)(
                -14,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                mono_font.as_ptr(),
            )
        };

        for (hit, widget) in &st.hits {
            let align_flag = |a: TextAlign| match a {
                TextAlign::Left => DT_LEFT,
                TextAlign::Center => DT_CENTER,
                TextAlign::Right => DT_RIGHT,
            };
            match &widget.kind {
                WidgetKind::Label { style, align } => {
                    let (font, color) = match style.as_str() {
                        "title" => (f_title, pal.text_primary),
                        "accent" => (f_label, pal.text_accent),
                        "error" => (f_label, pal.text_error),
                        "mono" => (f_list, pal.text_muted),
                        "muted" => (f_label, pal.text_muted),
                        _ => (f_label, pal.text_primary),
                    };
                    unsafe {
                        (st.api.select_object)(mem_dc, font);
                        (st.api.set_text_color)(mem_dc, color);
                    }
                    let mut tr = Rect {
                        left: hit.x + 2,
                        top: hit.y,
                        right: hit.x + hit.w - 2,
                        bottom: hit.y + hit.h,
                    };
                    let wtxt = wide(&widget.text);
                    unsafe {
                        (st.api.draw_text)(
                            mem_dc,
                            wtxt.as_ptr(),
                            -1,
                            &mut tr,
                            align_flag(*align) | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                        );
                    }
                }
                WidgetKind::Display { align } => {
                    unsafe {
                        draw_rounded_box(&st.api, mem_dc, hit, 14, pal.card, pal.card_border)
                    };
                    unsafe {
                        (st.api.select_object)(mem_dc, f_disp);
                        (st.api.set_text_color)(mem_dc, pal.text_primary);
                    }
                    let mut tr = Rect {
                        left: hit.x + 14,
                        top: hit.y + 4,
                        right: hit.x + hit.w - 14,
                        bottom: hit.y + hit.h - 4,
                    };
                    let wtxt = wide(&widget.text);
                    unsafe {
                        (st.api.draw_text)(
                            mem_dc,
                            wtxt.as_ptr(),
                            -1,
                            &mut tr,
                            align_flag(*align) | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                        );
                    }
                }
                WidgetKind::Button { role, .. } => {
                    let hovered = st.hover_id.as_deref() == Some(hit.id.as_str());
                    let pressed = st.pressed_id.as_deref() == Some(hit.id.as_str());
                    let (fill, fg) = match role.as_str() {
                        "accent" => (
                            if hovered {
                                pal.btn_accent_hover
                            } else {
                                pal.btn_accent
                            },
                            colorref(255, 255, 255),
                        ),
                        "op" => (
                            if hovered {
                                pal.btn_op_hover
                            } else {
                                pal.btn_op
                            },
                            pal.text_accent,
                        ),
                        "fn" => (
                            if hovered {
                                pal.btn_fn_hover
                            } else {
                                pal.btn_fn
                            },
                            pal.text_muted,
                        ),
                        "danger" => (
                            if hovered {
                                pal.btn_danger_hover
                            } else {
                                pal.btn_danger
                            },
                            pal.text_error,
                        ),
                        "num" => (
                            if hovered {
                                pal.btn_num_hover
                            } else {
                                pal.btn_num
                            },
                            pal.text_primary,
                        ),
                        _ => (
                            if hovered {
                                pal.btn_default_hover
                            } else {
                                pal.btn_default
                            },
                            pal.text_primary,
                        ),
                    };
                    let shift = if pressed { 1 } else { 0 };
                    let shifted = HitRect {
                        x: hit.x + shift,
                        y: hit.y + shift,
                        w: hit.w - shift * 2,
                        h: hit.h - shift * 2,
                        ..hit.clone()
                    };
                    unsafe {
                        draw_rounded_box(&st.api, mem_dc, &shifted, 12, fill, pal.btn_border);
                    }
                    unsafe {
                        (st.api.select_object)(mem_dc, f_btn);
                        (st.api.set_text_color)(mem_dc, fg);
                    }
                    let mut tr = Rect {
                        left: shifted.x + 4,
                        top: shifted.y,
                        right: shifted.x + shifted.w - 4,
                        bottom: shifted.y + shifted.h,
                    };
                    let wtxt = wide(&widget.text);
                    unsafe {
                        (st.api.draw_text)(
                            mem_dc,
                            wtxt.as_ptr(),
                            -1,
                            &mut tr,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );
                    }
                }
                WidgetKind::List { items, scroll } => {
                    unsafe {
                        draw_rounded_box(&st.api, mem_dc, hit, 12, pal.card, pal.card_border)
                    };
                    unsafe { (st.api.select_object)(mem_dc, f_list) };
                    let row_h = 22;
                    let visible = ((hit.h - 10) / row_h).max(1) as usize;
                    if items.is_empty() {
                        unsafe { (st.api.set_text_color)(mem_dc, pal.text_muted) };
                        let mut tr = Rect {
                            left: hit.x + 12,
                            top: hit.y + 6,
                            right: hit.x + hit.w - 12,
                            bottom: hit.y + hit.h - 6,
                        };
                        let wtxt = wide("(history tape appears here)");
                        unsafe {
                            (st.api.draw_text)(
                                mem_dc,
                                wtxt.as_ptr(),
                                -1,
                                &mut tr,
                                DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
                            );
                        }
                    } else {
                        unsafe { (st.api.set_text_color)(mem_dc, pal.text_muted) };
                        let start = (*scroll).min(items.len().saturating_sub(1));
                        for (row_idx, item) in items.iter().skip(start).take(visible).enumerate() {
                            let ry = hit.y + 5 + (row_idx as i32) * row_h;
                            let mut tr = Rect {
                                left: hit.x + 12,
                                top: ry,
                                right: hit.x + hit.w - 12,
                                bottom: ry + row_h,
                            };
                            if start + row_idx + 1 == items.len() {
                                unsafe { (st.api.set_text_color)(mem_dc, pal.text_primary) };
                            }
                            let wtxt = wide(item);
                            unsafe {
                                (st.api.draw_text)(
                                    mem_dc,
                                    wtxt.as_ptr(),
                                    -1,
                                    &mut tr,
                                    DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                                );
                            }
                        }
                    }
                }
                WidgetKind::Progress { value } => {
                    unsafe {
                        draw_rounded_box(&st.api, mem_dc, hit, 10, pal.prog_track, pal.card_border);
                    }
                    let fill_w = ((hit.w as f64) * value.clamp(0.0, 1.0)).round() as i32;
                    if fill_w > 6 {
                        let fill_hit = HitRect {
                            w: fill_w,
                            ..hit.clone()
                        };
                        unsafe {
                            draw_rounded_box(
                                &st.api,
                                mem_dc,
                                &fill_hit,
                                10,
                                pal.prog_fill,
                                pal.prog_fill,
                            );
                        }
                    }
                }
            }
        }

        unsafe {
            (st.api.bit_blt)(hdc, 0, 0, cw, ch, mem_dc, 0, 0, SRCCOPY);
            (st.api.select_object)(mem_dc, old_bmp);
            (st.api.delete_object)(f_label);
            (st.api.delete_object)(f_title);
            (st.api.delete_object)(f_disp);
            (st.api.delete_object)(f_btn);
            (st.api.delete_object)(f_list);
            (st.api.delete_object)(mem_bmp);
            (st.api.delete_dc)(mem_dc);
            (st.api.end_paint)(hwnd, &ps);
        }
    }

    unsafe fn draw_rounded_box(
        api: &WinApi,
        dc: isize,
        rect: &HitRect,
        radius: i32,
        fill: u32,
        border: u32,
    ) {
        let brush = unsafe { (api.create_solid_brush)(fill) };
        let pen = unsafe { (api.create_pen)(PS_SOLID, 1, border) };
        let old_b = unsafe { (api.select_object)(dc, brush) };
        let old_p = unsafe { (api.select_object)(dc, pen) };
        unsafe {
            (api.round_rect)(
                dc,
                rect.x,
                rect.y,
                rect.x + rect.w,
                rect.y + rect.h,
                radius,
                radius,
            );
            (api.select_object)(dc, old_b);
            (api.select_object)(dc, old_p);
            (api.delete_object)(brush);
            (api.delete_object)(pen);
        }
    }

    fn hit_at(hits: &[(HitRect, WidgetNode)], x: i32, y: i32) -> Option<&HitRect> {
        hits.iter().find_map(|(h, w)| {
            let interactive = matches!(w.kind, WidgetKind::Button { .. } | WidgetKind::List { .. });
            if interactive && x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + h.h {
                Some(h)
            } else {
                None
            }
        })
    }

    unsafe fn read_clipboard_text(api: &WinApi, hwnd: isize) -> Option<String> {
        if unsafe { (api.open_clipboard)(hwnd) } == 0 {
            return None;
        }
        let mut out = None;
        let handle = unsafe { (api.get_clipboard_data)(CF_UNICODETEXT) };
        if handle != 0 {
            let ptr = unsafe { (api.global_lock)(handle) } as *const u16;
            if !ptr.is_null() {
                let mut len = 0usize;
                while unsafe { *ptr.add(len) } != 0 && len < 16384 {
                    len += 1;
                }
                let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
                out = Some(String::from_utf16_lossy(slice));
                unsafe { (api.global_unlock)(handle) };
            }
        }
        unsafe { (api.close_clipboard)() };
        out
    }

    unsafe fn write_clipboard_text(api: &WinApi, hwnd: isize, text: &str) {
        if unsafe { (api.open_clipboard)(hwnd) } == 0 {
            return;
        }
        unsafe { (api.empty_clipboard)() };
        let w = wide(text);
        let bytes = w.len() * 2;
        let hmem = unsafe { (api.global_alloc)(GMEM_MOVEABLE, bytes) };
        if hmem != 0 {
            let dst = unsafe { (api.global_lock)(hmem) } as *mut u16;
            if !dst.is_null() {
                unsafe { std::ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len()) };
                unsafe { (api.global_unlock)(hmem) };
                unsafe { (api.set_clipboard_data)(CF_UNICODETEXT, hmem) };
            }
        }
        unsafe { (api.close_clipboard)() };
    }

    fn key_name(vk: usize) -> Option<&'static str> {
        match vk {
            0x08 => Some("Backspace"),
            0x09 => Some("Tab"),
            0x0D => Some("Enter"),
            0x1B => Some("Escape"),
            0x24 => Some("Home"),
            0x23 => Some("End"),
            0x25 => Some("Left"),
            0x26 => Some("Up"),
            0x27 => Some("Right"),
            0x28 => Some("Down"),
            0x2E => Some("Delete"),
            _ => None,
        }
    }

    struct WndProcApi {
        _user32: Library,
        get_window_long_ptr: unsafe extern "system" fn(isize, i32) -> isize,
        def_window_proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
    }
    unsafe impl Send for WndProcApi {}
    unsafe impl Sync for WndProcApi {}

    static WNDPROC_API: std::sync::OnceLock<Option<WndProcApi>> = std::sync::OnceLock::new();

    fn wndproc_api() -> Option<&'static WndProcApi> {
        WNDPROC_API
            .get_or_init(|| {
                let u32_lib = unsafe { Library::new("user32.dll") }.ok()?;
                let get_ptr = *unsafe {
                    u32_lib.get::<unsafe extern "system" fn(isize, i32) -> isize>(
                        b"GetWindowLongPtrW\0",
                    )
                }
                .ok()?;
                let def_fn = *unsafe {
                    u32_lib.get::<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>(
                        b"DefWindowProcW\0",
                    )
                }
                .ok()?;
                Some(WndProcApi {
                    _user32: u32_lib,
                    get_window_long_ptr: get_ptr,
                    def_window_proc: def_fn,
                })
            })
            .as_ref()
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: isize,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        let Some(wapi) = wndproc_api() else {
            return 0;
        };
        let st_ptr = unsafe {
            (wapi.get_window_long_ptr)(hwnd, GWLP_USERDATA) as *const RefCell<SharedWindowState>
        };
        if st_ptr.is_null() {
            return unsafe { (wapi.def_window_proc)(hwnd, msg, wparam, lparam) };
        }
        let st_cell = unsafe { &*st_ptr };
        let Ok(mut st) = st_cell.try_borrow_mut() else {
            return unsafe { (wapi.def_window_proc)(hwnd, msg, wparam, lparam) };
        };

        let l_x = (lparam & 0xFFFF) as i16 as i32;
        let l_y = ((lparam >> 16) & 0xFFFF) as i16 as i32;

        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                unsafe { paint_window(hwnd, &mut st) };
                0
            }
            WM_SIZE => {
                unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                0
            }
            WM_MOUSEMOVE => {
                if !st.tracking_mouse {
                    let mut tme = TrackMouseEventStruct {
                        cb_size: std::mem::size_of::<TrackMouseEventStruct>() as u32,
                        dw_flags: TME_LEAVE,
                        hwnd_track: hwnd,
                        dw_hover_time: 0,
                    };
                    unsafe { (st.api.track_mouse_event)(&mut tme) };
                    st.tracking_mouse = true;
                }
                let next_hover = hit_at(&st.hits, l_x, l_y).map(|h| h.id.clone());
                let cur = unsafe {
                    (st.api.load_cursor)(
                        0,
                        if next_hover.is_some() {
                            IDC_HAND
                        } else {
                            IDC_ARROW
                        },
                    )
                };
                if cur != 0 {
                    unsafe { (st.api.set_cursor)(cur) };
                }
                if next_hover != st.hover_id {
                    st.hover_id = next_hover;
                    unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                }
                0
            }
            WM_MOUSELEAVE => {
                st.tracking_mouse = false;
                if st.hover_id.is_some() || st.pressed_id.is_some() {
                    st.hover_id = None;
                    st.pressed_id = None;
                    unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                }
                0
            }
            WM_LBUTTONDOWN => {
                st.pressed_id = hit_at(&st.hits, l_x, l_y).map(|h| h.id.clone());
                if st.pressed_id.is_some() {
                    unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                }
                0
            }
            WM_LBUTTONUP => {
                let target = hit_at(&st.hits, l_x, l_y).cloned();
                let was = st.pressed_id.take();
                if let Some(hit) = target
                    && was.as_deref() == Some(hit.id.as_str())
                {
                    if hit.is_list {
                        let row_idx = ((l_y - hit.y - 5).max(0) / 22) as usize + hit.scroll;
                        let clicked_item =
                            st.hits
                                .iter()
                                .find(|(h, _)| h.id == hit.id)
                                .and_then(|(_, widget)| match &widget.kind {
                                    WidgetKind::List { items, .. } => items.get(row_idx).cloned(),
                                    _ => None,
                                });
                        if let Some(item_text) = clicked_item {
                            st.events.push_back(UiEvent::ListClick {
                                id: hit.id,
                                index: row_idx,
                                text: item_text,
                            });
                        }
                    } else {
                        st.events.push_back(UiEvent::Click { id: hit.id });
                    }
                }
                unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                0
            }
            WM_MOUSEWHEEL => {
                let delta = ((wparam >> 16) as u16 as i16) as i32;
                let step = if delta > 0 { -1isize } else { 1isize };
                for entry in &mut st.layout.entries {
                    if let LayoutEntry::Single(w) = entry
                        && let WidgetKind::List { items, scroll } = &mut w.kind
                        && !items.is_empty()
                    {
                        let next = (*scroll as isize + step)
                            .clamp(0, items.len().saturating_sub(1) as isize);
                        *scroll = next as usize;
                    }
                }
                unsafe { (st.api.invalidate_rect)(hwnd, std::ptr::null(), 0) };
                0
            }
            WM_KEYDOWN => {
                let ctrl = unsafe { (st.api.get_key_state)(0x11) } < 0;
                if ctrl && wparam == b'C' as usize {
                    if let Some(disp) = st.layout.entries.iter().find_map(|e| match e {
                        LayoutEntry::Single(w) if matches!(w.kind, WidgetKind::Display { .. }) => {
                            Some(w.text.clone())
                        }
                        _ => None,
                    }) {
                        unsafe { write_clipboard_text(&st.api, hwnd, &disp) };
                    }
                } else if ctrl && wparam == b'V' as usize {
                    if let Some(pasted) = unsafe { read_clipboard_text(&st.api, hwnd) } {
                        st.events.push_back(UiEvent::Paste { text: pasted });
                    }
                } else if let Some(k) = key_name(wparam) {
                    st.events.push_back(UiEvent::Key {
                        key: k.into(),
                        ctrl,
                    });
                }
                0
            }
            WM_CHAR => {
                let code = wparam as u32;
                if code >= 0x20
                    && let Some(ch) = char::from_u32(code)
                {
                    st.events.push_back(UiEvent::Char { ch });
                }
                0
            }
            WM_CLOSE => {
                st.closed = true;
                let dw = st.api.destroy_window;
                drop(st);
                unsafe { dw(hwnd) };
                0
            }
            WM_DESTROY => {
                st.closed = true;
                unsafe { (st.api.post_quit_message)(0) };
                0
            }
            _ => {
                let def = st.api.def_window_proc;
                drop(st);
                unsafe { def(hwnd, msg, wparam, lparam) }
            }
        }
    }

    pub(crate) fn run_win32_app(
        vm: &mut crate::vm::Vm,
        layout: GuiLayout,
        handler: &Val,
    ) -> Result<(), String> {
        detach_explorer_console();
        let api = unsafe { WinApi::load()? };
        let class_name = wide("KovrekNativeWindowClass");
        let title_w = wide(&layout.title);
        let dark_mode = layout.theme == ThemeMode::Dark;
        let init_w = layout.width;
        let init_h = layout.height;

        let wc = WndClassExW {
            cb_size: std::mem::size_of::<WndClassExW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfn_wnd_proc: wnd_proc,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: 0,
            h_icon: 0,
            h_cursor: unsafe { (api.load_cursor)(0, IDC_ARROW) },
            hbr_background: 0,
            lpsz_menu_name: std::ptr::null(),
            lpsz_class_name: class_name.as_ptr(),
            h_icon_sm: 0,
        };
        unsafe { (api.register_class_ex)(&wc) };

        let mut win_rect = Rect {
            left: 0,
            top: 0,
            right: init_w,
            bottom: init_h,
        };
        unsafe { (api.adjust_window_rect_ex)(&mut win_rect, WS_OVERLAPPEDWINDOW, 0, 0) };
        let full_w = win_rect.right - win_rect.left;
        let full_h = win_rect.bottom - win_rect.top;

        let state = Rc::new(RefCell::new(SharedWindowState {
            api,
            layout,
            hits: Vec::new(),
            hover_id: None,
            pressed_id: None,
            tracking_mouse: false,
            closed: false,
            events: VecDeque::new(),
        }));
        let state_ptr = Rc::as_ptr(&state) as isize;

        fn apply_patch_and_refresh(state: &RefCell<SharedWindowState>, hwnd: isize, patch: &Val) {
            let (new_title, changed, close_req, set_txt, inv, dw) = {
                let mut st = state.borrow_mut();
                let prev_title = st.layout.title.clone();
                let (close_req, changed) = apply_layout_updates(&mut st.layout, patch);
                let title_opt = if st.layout.title != prev_title {
                    Some(wide(&st.layout.title))
                } else {
                    None
                };
                (
                    title_opt,
                    changed,
                    close_req,
                    st.api.set_window_text,
                    st.api.invalidate_rect,
                    st.api.destroy_window,
                )
            };
            if let Some(tw) = new_title {
                unsafe { set_txt(hwnd, tw.as_ptr()) };
            }
            if changed {
                unsafe { inv(hwnd, std::ptr::null(), 0) };
            }
            if close_req {
                unsafe { dw(hwnd) };
            }
        }

        let create_fn = state.borrow().api.create_window_ex;
        let hwnd = unsafe {
            create_fn(
                0,
                class_name.as_ptr(),
                title_w.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                full_w,
                full_h,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        if hwnd == 0 {
            return Err("gui_app: CreateWindowExW failed".into());
        }
        let (set_ptr_fn, show_fn, update_fn) = {
            let st = state.borrow();
            (
                st.api.set_window_long_ptr,
                st.api.show_window,
                st.api.update_window,
            )
        };
        unsafe {
            set_ptr_fn(hwnd, GWLP_USERDATA, state_ptr);
            enable_dark_frame(hwnd, dark_mode);
            show_fn(hwnd, SW_SHOW);
            update_fn(hwnd);
        }

        // Send initial lifecycle event so app can populate initial widgets dynamically.
        let init_ev = Val::Map(vec![(Val::Str("kind".into()), Val::Str("init".into()))]);
        if let Ok(patch) = vm.call_val_kw_sync(handler, vec![init_ev], vec![]) {
            apply_patch_and_refresh(&state, hwnd, &patch);
        }

        loop {
            let mut msg = Msg::default();
            let (peek_fn, translate_fn, dispatch_fn) = {
                let st = state.borrow();
                (
                    st.api.peek_message,
                    st.api.translate_message,
                    st.api.dispatch_message,
                )
            };
            loop {
                let has_msg = unsafe { peek_fn(&mut msg, 0, 0, 0, PM_REMOVE) != 0 };
                if !has_msg {
                    break;
                }
                if msg.message == 0x0012 {
                    state.borrow_mut().closed = true;
                    break;
                }
                unsafe {
                    translate_fn(&msg);
                    dispatch_fn(&msg);
                }
            }

            if state.borrow().closed {
                break;
            }

            let pending: Vec<UiEvent> = {
                let mut st = state.borrow_mut();
                st.events.drain(..).collect()
            };
            for ev in pending {
                let ev_val = ui_event_to_val(&ev);
                match vm.call_val_kw_sync(handler, vec![ev_val], vec![]) {
                    Ok(patch) => apply_patch_and_refresh(&state, hwnd, &patch),
                    Err(err) => {
                        let inv = {
                            let mut st = state.borrow_mut();
                            for entry in &mut st.layout.entries {
                                if let LayoutEntry::Single(w) = entry
                                    && matches!(w.kind, WidgetKind::Label { .. })
                                {
                                    w.text = format!("Error: {err}");
                                    break;
                                }
                            }
                            st.api.invalidate_rect
                        };
                        unsafe { inv(hwnd, std::ptr::null(), 0) };
                    }
                }
            }

            if state.borrow().closed {
                break;
            }
            let wm_wait = state.borrow().api.wait_message;
            unsafe { wm_wait() };
        }
        unsafe { set_ptr_fn(hwnd, GWLP_USERDATA, 0) };
        Ok(())
    }
}

pub(crate) fn run_gui_app(
    vm: &mut crate::vm::Vm,
    spec: &Val,
    handler: &Val,
) -> Result<Val, String> {
    let layout = parse_gui_layout(spec)?;
    #[cfg(windows)]
    {
        win32_backend::run_win32_app(vm, layout, handler)?;
        Ok(Val::Null)
    }
    #[cfg(not(windows))]
    {
        let _ = (vm, layout, handler);
        Err("gui_app native window currently requires Windows desktop".into())
    }
}
