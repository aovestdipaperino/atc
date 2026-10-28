//! Always-on-top countdown overlay for the main macOS screen.
//!
//! Usage: `atc <minutes>` starts a countdown (fractions allowed, e.g. `0.5`),
//! `atc stop` dismisses any running countdown. The overlay is click-through,
//! shows on every Space (including over full-screen apps) and detaches from
//! the terminal so the shell is free immediately.

use std::cell::Cell;
use std::env;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly as _};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBox, NSBoxType, NSColor,
    NSFont, NSFontWeightBold, NSScreen, NSSound, NSStatusWindowLevel, NSTextAlignment, NSTextField,
    NSTitlePosition, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

/// Marks the detached child process so it does not fork again.
const CHILD_ENV: &str = "ATC_DETACHED";
const WIDTH: f64 = 220.0;
const HEIGHT: f64 = 80.0;
const MARGIN: f64 = 16.0;
/// How long the overlay keeps flashing after reaching zero before exiting.
const FINISHED_LINGER: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd] if cmd == "stop" => {
            stop_others();
            ExitCode::SUCCESS
        }
        [mins] => match parse_minutes(mins) {
            Some(total) if env::var_os(CHILD_ENV).is_some() => {
                run_overlay(total);
                ExitCode::SUCCESS
            }
            Some(_) => detach(mins),
            None => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: atc <minutes>   start a countdown (e.g. 25, 0.5)");
    eprintln!("       atc stop        dismiss the running countdown");
    ExitCode::FAILURE
}

fn parse_minutes(s: &str) -> Option<Duration> {
    let mins: f64 = s.parse().ok()?;
    (mins.is_finite() && mins > 0.0).then(|| Duration::from_secs_f64(mins * 60.0))
}

/// Replaces any running countdown and re-launches this binary in the background.
fn detach(mins: &str) -> ExitCode {
    stop_others();
    let Ok(exe) = env::current_exe() else {
        eprintln!("atc: cannot locate own executable");
        return ExitCode::FAILURE;
    };
    let spawned = Command::new(exe)
        .arg(mins)
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // A new process group keeps the overlay alive when the terminal closes.
        .process_group(0)
        .spawn();
    match spawned {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("atc: failed to start overlay: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Terminates every other `atc` process belonging to the current user.
fn stop_others() {
    let Ok(out) = Command::new("pgrep").args(["-x", "atc"]).output() else {
        return;
    };
    let me = std::process::id().to_string();
    for pid in String::from_utf8_lossy(&out.stdout).lines() {
        if pid != me {
            let _ = Command::new("kill").arg(pid).status();
        }
    }
}

fn format_remaining(left: Duration) -> String {
    // Round up so the display reads 00:01 during the final second, not 00:00.
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn run_overlay(total: Duration) {
    let mtm = MainThreadMarker::new().expect("AppKit must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Accessory: no Dock icon, no menu bar, never steals focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let window = build_window(mtm);
    let label = build_label(mtm, &format_remaining(total));
    let content = window
        .contentView()
        .expect("borderless window has a content view");
    content.addSubview(&label);
    window.orderFrontRegardless();

    let deadline = Instant::now() + total;
    let finished_at: Cell<Option<Instant>> = Cell::new(None);
    let tick = RcBlock::new(move |_timer| {
        let now = Instant::now();
        let left = deadline.saturating_duration_since(now);
        label.setStringValue(&NSString::from_str(&format_remaining(left)));

        if left.is_zero() {
            let done = finished_at.get().unwrap_or_else(|| {
                label.setTextColor(Some(&NSColor::systemRedColor()));
                if let Some(sound) = NSSound::soundNamed(&NSString::from_str("Glass")) {
                    sound.play();
                }
                finished_at.set(Some(now));
                now
            });
            let elapsed = now - done;
            // Blink twice a second until the linger period is over.
            let visible = (elapsed.as_millis() / 500).is_multiple_of(2);
            window.setAlphaValue(if visible { 1.0 } else { 0.25 });
            if elapsed >= FINISHED_LINGER {
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        } else if left < Duration::from_secs(60) {
            label.setTextColor(Some(&NSColor::systemOrangeColor()));
        }
    });
    // SAFETY: the block only touches main-thread objects and the timer is
    // scheduled on the main run loop, so it always runs on the main thread.
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.25, true, &tick);
    }
    app.run();
}

/// Creates the click-through, always-on-top panel in the primary screen's top-right corner.
fn build_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    // screens[0] is always the screen holding the menu bar, unlike NSScreen::mainScreen,
    // which follows keyboard focus.
    let screen = NSScreen::screens(mtm)
        .firstObject()
        .expect("at least one screen is attached");
    let visible = screen.visibleFrame();
    let frame = NSRect::new(
        NSPoint::new(
            visible.origin.x + visible.size.width - WIDTH - MARGIN,
            visible.origin.y + visible.size.height - HEIGHT - MARGIN,
        ),
        NSSize::new(WIDTH, HEIGHT),
    );

    // SAFETY: standard initializer with valid arguments; released-when-closed is
    // disabled right after so Retained keeps sole ownership.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setHasShadow(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setIgnoresMouseEvents(true);
    window.setLevel(NSStatusWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );

    let background = NSBox::initWithFrame(
        NSBox::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
    );
    background.setBoxType(NSBoxType::Custom);
    background.setTitlePosition(NSTitlePosition::NoTitle);
    background.setBorderWidth(0.0);
    background.setCornerRadius(16.0);
    background.setFillColor(&NSColor::colorWithWhite_alpha(0.0, 0.65));
    window.setContentView(Some(&background));
    window
}

fn build_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    // SAFETY: NSFontWeightBold is an immutable AppKit constant.
    let weight = unsafe { NSFontWeightBold };
    label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
        44.0, weight,
    )));
    label.setTextColor(Some(&NSColor::whiteColor()));
    label.setAlignment(NSTextAlignment::Center);
    let h = label.fittingSize().height;
    label.setFrame(NSRect::new(
        NSPoint::new(0.0, (HEIGHT - h) / 2.0),
        NSSize::new(WIDTH, h),
    ));
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_minutes_and_hours() {
        assert_eq!(format_remaining(Duration::from_mins(25)), "25:00");
        assert_eq!(format_remaining(Duration::from_millis(500)), "00:01");
        assert_eq!(format_remaining(Duration::ZERO), "00:00");
        assert_eq!(format_remaining(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn parses_minutes() {
        assert_eq!(parse_minutes("0.5"), Some(Duration::from_secs(30)));
        assert_eq!(parse_minutes("0"), None);
        assert_eq!(parse_minutes("-3"), None);
        assert_eq!(parse_minutes("abc"), None);
    }
}
