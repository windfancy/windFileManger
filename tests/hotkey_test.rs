//! 快捷键焦点链与键名实证（Slint 官方 testing backend，dev-dependency 仅供本地测试）。
//!
//! 真实 Windows 下 winit 后端的 KeyEvent.text 取值（源码级考证）：
//! - 功能键：for_each_keys 映射表内部 unicode 值，如 F2 = "\u{F705}"、Delete = "\u{7f}"、
//!   Backspace = "\u{8}"、Escape = "\u{1b}"、LeftArrow = "\u{F702}"
//! - Ctrl+字母：text = 控制字符（Ctrl+C = "\u{3}"），text_without_modifiers 才是 "c"
//!   （winitwindowadapter.rs：text 非空时不做 without_modifiers 回填）
//!
//! 一个 #[test] 串行跑完所有用例：init_no_event_loop 全局只能设一次 platform。

slint::include_modules!();

use i_slint_backend_testing as slint_testing;
use std::cell::RefCell;
use std::rc::Rc;

/// 构建窗口并挂钩 hotkey 回调，返回 (弱引用, 收到的键名列表)
fn setup() -> (slint::Weak<AppWindow>, Rc<RefCell<Vec<String>>>) {
    slint_testing::init_no_event_loop();
    let app = AppWindow::new().expect("AppWindow 创建失败");
    let got = Rc::new(RefCell::new(Vec::<String>::new()));
    let g2 = got.clone();
    app.global::<Logic>().on_hotkey(move |k| {
        g2.borrow_mut().push(k.to_string());
    });
    app.show().expect("show 失败");
    (app.as_weak(), got)
}

fn send_key(app: &AppWindow, text: &str) {
    slint_testing::send_keyboard_key_text(app, &text.into(), true);
}

#[test]
fn hotkey_focus_chain_and_key_names() {
    let (weak, got) = setup();
    let app = weak.unwrap();
    let l = app.global::<Logic>();

    // 用例 1：焦点链——show() 后（未点击任何组件）按 F2，FocusScope 应收到键
    send_key(&app, "\u{F705}"); // F2 内部值
    assert_eq!(
        got.borrow().last().map(String::as_str),
        Some("F2"),
        "show() 后 F2 未到达 FocusScope → forward-focus 失效"
    );

    // 用例 2：Delete（\u{7f}）
    send_key(&app, "\u{7f}");
    assert_eq!(got.borrow().last().map(String::as_str), Some("Delete"));

    // 用例 3：Ctrl+C（真实 Windows text = 控制字符 \u{3}）。dispatch_event 注入不带
    // modifiers（testing API 限制），Slint 端 hotkey-name 原样透传 → Rust 端收到 "\u{3}"；
    // 真实组合 modifiers.control=true 时组装 "Ctrl+\u{3}"，由 Rust normalize_key 换算（见 main.rs 单测）
    send_key(&app, "\u{3}");
    assert_eq!(got.borrow().last().map(String::as_str), Some("\u{3}"));

    // 用例 4：普通字符（无修饰）原样传递
    send_key(&app, "x");
    assert_eq!(got.borrow().last().map(String::as_str), Some("x"));

    // 用例 5：Escape 到达（Rust 分派内部保留处理，不进动作匹配）
    send_key(&app, "\u{1b}");
    assert_eq!(got.borrow().last().map(String::as_str), Some("Escape"));

    // 用例 6：捕获态透传 —— capture-action 非空时任何键都带 "__cap:" 前缀
    l.set_capture_action("delete".into());
    send_key(&app, "\u{1b}"); // Escape
    assert_eq!(
        got.borrow().last().map(String::as_str),
        Some("__cap:Escape"),
        "捕获态前缀缺失"
    );
    l.set_capture_action("".into());
}
