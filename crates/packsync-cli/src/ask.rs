//! A yes/no question in a native window, for hooks: Prism runs them without a terminal, so a question
//! printed to stderr would only reach its log. `None` when no dialog can be shown (then the caller
//! decides what an unanswered question means).

/// Ask `question`; `yes`/`no` are the button labels where the platform allows custom ones.
pub fn yes_no(title: &str, question: &str, yes: &str, no: &str) -> Option<bool> {
    platform(title, question, yes, no)
}

#[cfg(windows)]
fn platform(title: &str, question: &str, _yes: &str, _no: &str) -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IDYES, MB_ICONWARNING, MB_TOPMOST, MB_YESNO, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (t, q) = (wide(title), wide(question));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call; no owner window.
    let r =
        unsafe { MessageBoxW(std::ptr::null_mut(), q.as_ptr(), t.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_TOPMOST) };
    match r {
        0 => None,
        r => Some(r == IDYES),
    }
}

#[cfg(target_os = "macos")]
fn platform(title: &str, question: &str, yes: &str, no: &str) -> Option<bool> {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display dialog \"{}\" with title \"{}\" buttons {{\"{}\", \"{}\"}} default button 2 with icon caution",
        esc(question),
        esc(title),
        esc(no),
        esc(yes)
    );
    let out = std::process::Command::new("osascript").args(["-e", &script]).output().ok()?;
    // "Cancel"-like buttons end with an error status; the chosen button is printed otherwise
    Some(out.status.success() && String::from_utf8_lossy(&out.stdout).contains(yes))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform(title: &str, question: &str, yes: &str, no: &str) -> Option<bool> {
    use std::process::Command;
    // zenity (GNOME and most desktops), then kdialog (KDE): exit 0 = yes, 1 = no, anything else = no dialog
    let tries: [(&str, Vec<String>); 2] = [
        (
            "zenity",
            vec![
                "--question".into(),
                format!("--title={title}"),
                format!("--text={question}"),
                format!("--ok-label={yes}"),
                format!("--cancel-label={no}"),
                "--icon=dialog-warning".into(),
            ],
        ),
        (
            "kdialog",
            vec![
                "--title".into(),
                title.into(),
                "--yes-label".into(),
                yes.into(),
                "--no-label".into(),
                no.into(),
                "--warningyesno".into(),
                question.into(),
            ],
        ),
    ];
    for (prog, args) in tries {
        if let Ok(status) = Command::new(prog).args(&args).status() {
            match status.code() {
                Some(0) => return Some(true),
                Some(1) => return Some(false),
                _ => {}
            }
        }
    }
    None
}

/// Ask the person to pick one of two versions, or neither: `Some(Some(0))` / `Some(Some(1))` for the
/// first / second choice, `Some(None)` for "cancel", `None` when no dialog can be shown.
pub fn choose(title: &str, question: &str, first: &str, second: &str, cancel: &str) -> Option<Option<usize>> {
    choose_platform(title, question, first, second, cancel)
}

#[cfg(windows)]
fn choose_platform(title: &str, question: &str, first: &str, second: &str, _cancel: &str) -> Option<Option<usize>> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDNO, IDYES, MB_ICONWARNING, MB_TOPMOST, MB_YESNOCANCEL, MessageBoxW,
    };
    // MessageBox has fixed button labels: say which is which
    let text = format!("{question}\n\nYes: {first}\nNo: {second}\nCancel: don't play now");
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (t, q) = (wide(title), wide(&text));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call; no owner window.
    let r = unsafe {
        MessageBoxW(std::ptr::null_mut(), q.as_ptr(), t.as_ptr(), MB_YESNOCANCEL | MB_ICONWARNING | MB_TOPMOST)
    };
    match r {
        0 => None,
        IDYES => Some(Some(0)),
        IDNO => Some(Some(1)),
        _ => Some(None),
    }
}

#[cfg(target_os = "macos")]
fn choose_platform(title: &str, question: &str, first: &str, second: &str, cancel: &str) -> Option<Option<usize>> {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display dialog \"{}\" with title \"{}\" buttons {{\"{}\", \"{}\", \"{}\"}} default button 3 with icon caution",
        esc(question),
        esc(title),
        esc(cancel),
        esc(second),
        esc(first)
    );
    let out = std::process::Command::new("osascript").args(["-e", &script]).output().ok()?;
    let picked = String::from_utf8_lossy(&out.stdout).into_owned();
    Some(if picked.contains(first) {
        Some(0)
    } else if picked.contains(second) {
        Some(1)
    } else {
        None
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn choose_platform(title: &str, question: &str, first: &str, second: &str, cancel: &str) -> Option<Option<usize>> {
    use std::process::Command;
    // zenity: OK = first, the extra button prints its label, Cancel = neither
    if let Ok(out) = Command::new("zenity")
        .args([
            "--question",
            &format!("--title={title}"),
            &format!("--text={question}"),
            &format!("--ok-label={first}"),
            &format!("--extra-button={second}"),
            &format!("--cancel-label={cancel}"),
            "--icon=dialog-warning",
        ])
        .output()
    {
        match out.status.code() {
            Some(0) => return Some(Some(0)),
            Some(1) if String::from_utf8_lossy(&out.stdout).trim() == second => return Some(Some(1)),
            Some(1) => return Some(None),
            _ => {}
        }
    }
    // kdialog: yes / no / cancel
    if let Ok(status) = Command::new("kdialog")
        .args(["--title", title, "--yes-label", first, "--no-label", second, "--cancel-label", cancel])
        .args(["--warningyesnocancel", question])
        .status()
    {
        match status.code() {
            Some(0) => return Some(Some(0)),
            Some(1) => return Some(Some(1)),
            Some(2) => return Some(None),
            _ => {}
        }
    }
    None
}
