//! Keyboard handling, including the one genuinely awkward part of moving this
//! screen into raw mode.
//!
//! Dropping files onto a console window works because Windows types the paths
//! in for you. In raw mode that arrives as a burst of ordinary character
//! events, which would otherwise be read as commands - a dropped
//! `"C:\clips\a.mp4"` would hit `c` for clear before anything else.
//!
//! Two things sort it out. Terminals that support bracketed paste (Windows
//! Terminal) send the whole run as one `Event::Paste`, which is unambiguous.
//! Older consoles do not, so a run of characters arriving faster than a person
//! can type is treated as pasted text instead of as commands. Either way the
//! text lands in the add prompt, where it can be checked before adding.

use std::io::{self, Write};
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;

use crate::app::{App, Confirm, Kind, Overlay, PromptKind, Screen};
use crate::ui::{Click, UiState};

/// No human produces a second keystroke this fast, so anything that does is
/// a paste or a drop.
const BURST_GAP: Duration = Duration::from_millis(15);

/// Reads and handles whatever the terminal has to say, waiting up to `timeout`
/// for the first event.
///
/// Returns whether anything was handled, so an idle screen is not redrawn ten
/// times a second for nothing.
pub fn pump(app: &mut App, ui: &mut UiState, timeout: Duration) -> io::Result<bool> {
    if !event::poll(timeout)? {
        return Ok(false);
    }
    loop {
        let event = event::read()?;
        dispatch(app, ui, event)?;
        if app.quit || !event::poll(Duration::ZERO)? {
            break;
        }
    }
    Ok(true)
}

fn dispatch(app: &mut App, ui: &mut UiState, event: Event) -> io::Result<()> {
    match event {
        Event::Paste(text) => {
            pasted(app, text);
            Ok(())
        }
        // Windows reports key releases as well as presses; acting on both
        // would run every command twice.
        Event::Key(key) if key.kind == KeyEventKind::Press => key_pressed(app, key),
        Event::Mouse(mouse) => {
            mouse_event(app, ui, mouse);
            Ok(())
        }
        _ => Ok(()),
    }
}

// ----------------------------------------------------------------------- mouse

fn mouse_event(app: &mut App, ui: &mut UiState, mouse: MouseEvent) {
    // Every mouse event carries a position, and the next redraw lights up
    // whatever is under it. Recorded before anything is acted on, so a button
    // clicked without a preceding move still shows as the one that was hit.
    ui.set_pointer(mouse.column, mouse.row);

    match mouse.kind {
        MouseEventKind::ScrollUp => scroll(app, -3),
        MouseEventKind::ScrollDown => scroll(app, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(click) = ui.hit(mouse.column, mouse.row) {
                clicked(app, click);
            }
        }
        _ => {}
    }
}

fn scroll(app: &mut App, delta: isize) {
    match app.overlay {
        Overlay::Menu(_) => app.menu_move(delta.signum()),
        // A prompt or a dialog has nothing to scroll, and moving the list
        // underneath it would be invisible.
        Overlay::None if matches!(app.screen, Screen::Browse) => app.move_cursor(delta),
        _ => {}
    }
}

/// Every click ends up in the same handler as the key it stands for, so the two
/// routes cannot drift apart.
fn clicked(app: &mut App, click: Click) {
    match click {
        Click::Row(index) => app.cursor_to(index),
        Click::MenuItem(index) => app.menu_pick(Some(index)),
        Click::Mark => app.toggle_mark(),
        Click::Remove => app.remove_selection(),
        Click::Back => app.dismiss_result(),
        Click::Answer(yes) => answer(app, yes),
        Click::Submit => app.submit_prompt(),
        Click::Cancel => app.close_overlay(),
        Click::Ignore => {}
        Click::Command(c) => match app.screen {
            Screen::Browse => command(app, c),
            Screen::Result(_) | Screen::Fetched(_) => result_command(app, c),
            // Cancelling a merge or a download stays on the keyboard: a stray
            // click should not be able to throw away work in progress.
            Screen::Merging(_) | Screen::Fetching(_) => {}
        },
    }
}

/// Mouse capture takes the terminal's own text selection away, so it has to be
/// possible to hand it back without leaving the program.
fn toggle_mouse(app: &mut App) {
    app.mouse = !app.mouse;
    let mut out = io::stdout();
    let _ = if app.mouse {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    };
    let _ = out.flush();

    if app.mouse {
        app.say("Mouse on - click a clip, or any button along the bottom.", Kind::Good);
    } else {
        app.say("Mouse off - you can select and copy text again. M turns it back on.", Kind::Good);
    }
}

/// Text that arrived all at once, whichever way the terminal delivered it.
fn pasted(app: &mut App, text: String) {
    let text = text.replace(['\r', '\n'], " ").trim().to_string();
    if text.is_empty() {
        return;
    }
    match &mut app.overlay {
        Overlay::Prompt(prompt) => prompt.buffer.push_str(&text),
        // Anywhere else, a drop means "add these".
        _ if matches!(app.screen, Screen::Browse) => app.prompt_add_with(text),
        _ => {}
    }
}

fn key_pressed(app: &mut App, key: KeyEvent) -> io::Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        if matches!(app.screen, Screen::Merging(_) | Screen::Fetching(_)) {
            app.request_cancel();
        } else {
            app.quit = true;
        }
        return Ok(());
    }

    match &app.overlay {
        Overlay::Prompt(_) => return prompt_key(app, key),
        Overlay::Menu(_) => {
            menu_key(app, key);
            return Ok(());
        }
        Overlay::Confirm(_) => {
            confirm_key(app, key);
            return Ok(());
        }
        Overlay::Help(_) => {
            // Any key at all dismisses a reference sheet.
            app.close_overlay();
            return Ok(());
        }
        Overlay::None => {}
    }

    match &app.screen {
        Screen::Browse => browse_key(app, key),
        Screen::Merging(_) | Screen::Fetching(_) => {
            if matches!(key.code, KeyCode::Esc) {
                app.request_cancel();
            }
            Ok(())
        }
        Screen::Result(_) | Screen::Fetched(_) => {
            result_key(app, key);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------- browse

fn browse_key(app: &mut App, key: KeyEvent) -> io::Result<()> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let reorder = shift || alt;

    match key.code {
        KeyCode::Up if reorder => app.move_clip(-1),
        KeyCode::Down if reorder => app.move_clip(1),
        KeyCode::Up => app.move_cursor(-1),
        KeyCode::Down => app.move_cursor(1),
        KeyCode::PageUp => app.move_cursor(-10),
        KeyCode::PageDown => app.move_cursor(10),
        KeyCode::Home => app.cursor_to(0),
        KeyCode::End => app.cursor_to(usize::MAX),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Delete | KeyCode::Backspace => app.remove_selection(),
        KeyCode::Enter => app.prompt_add(),
        KeyCode::Esc if app.marked_count() > 0 => {
            for entry in app.clips.iter_mut() {
                entry.marked = false;
            }
            app.say("Marks cleared.", Kind::Info);
        }
        KeyCode::Char(c) => {
            // Could be a command, could be the first character of a dropped
            // path. Gather anything arriving on its heels to find out.
            let text = gather_burst(c)?;
            let mut chars = text.chars();
            let single = chars.next().filter(|_| chars.next().is_none());
            match single {
                // One character, and not one that starts a path: a command.
                Some(c) if !starts_a_path(c) => command(app, c),
                _ => app.prompt_add_with(text),
            }
        }
        _ => {}
    }
    Ok(())
}

/// A quote is how Windows wraps a dropped path, and no command uses one.
fn starts_a_path(c: char) -> bool {
    c == '"' || c == '\''
}

/// Collects characters that keep arriving without a human-sized pause.
fn gather_burst(first: char) -> io::Result<String> {
    let mut text = String::from(first);
    while event::poll(BURST_GAP)? {
        match event::read()? {
            Event::Paste(chunk) => text.push_str(&chunk),
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char(c) => text.push(c),
                // Explorer's drop ends the line; so does a pasted path.
                KeyCode::Enter => break,
                _ => break,
            },
            _ => {}
        }
    }
    Ok(text)
}

fn command(app: &mut App, c: char) {
    // Shift-held letters reorder, matching shift+arrows. Checked before the
    // case-insensitive commands below, or K would also move the cursor.
    match c {
        'K' => return app.move_clip(-1),
        'J' => return app.move_clip(1),
        'G' => return app.cursor_to(usize::MAX),
        _ => {}
    }

    match c.to_ascii_lowercase() {
        'k' => app.move_cursor(-1),
        'j' => app.move_cursor(1),
        'a' | 'f' => app.prompt_add(),
        'c' => app.clear(),
        'd' => app.remove_selection(),
        'n' => app.sort_by_name(),
        'o' => app.prompt_output(),
        'q' => app.menu_quality(),
        't' => app.menu_target(),
        'e' => app.toggle_encoder(),
        'r' => {
            app.force_reencode = !app.force_reencode;
            let note = if app.force_reencode {
                "Forced re-encode on - every clip goes through the encoder."
            } else {
                "Forced re-encode off - clips that already match are copied."
            };
            app.say(note, Kind::Good);
        }
        'u' => app.prompt_fetch(),
        'm' => toggle_mouse(app),
        's' => app.request_merge(),
        'x' => app.quit = true,
        'g' => app.cursor_to(0),
        '?' => app.toggle_help(),
        _ => app.say(format!("Nothing bound to '{c}'."), Kind::Warn),
    }
}

// ---------------------------------------------------------------------- prompt

fn prompt_key(app: &mut App, key: KeyEvent) -> io::Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Overlay::Prompt(prompt) = &mut app.overlay else {
        return Ok(());
    };

    match key.code {
        KeyCode::Enter => app.submit_prompt(),
        KeyCode::Esc => app.close_overlay(),
        KeyCode::Backspace => {
            prompt.buffer.pop();
        }
        KeyCode::Char('u') if ctrl => prompt.buffer.clear(),
        KeyCode::Char('w') if ctrl => {
            // Drop the last whitespace-separated word, which for a list of
            // dropped paths means dropping the last path.
            let trimmed = prompt.buffer.trim_end();
            let cut = trimmed.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
            prompt.buffer.truncate(cut);
        }
        KeyCode::Char(c) => prompt.buffer.push(c),
        // A convenience for the add prompt: accept without reaching for Enter.
        KeyCode::Tab if prompt.kind == PromptKind::AddPaths => app.submit_prompt(),
        _ => {}
    }
    Ok(())
}

// ------------------------------------------------------------------ menu

fn menu_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => app.menu_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.menu_move(1),
        KeyCode::Enter | KeyCode::Char(' ') => app.menu_pick(None),
        KeyCode::Esc => app.close_overlay(),
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            app.menu_pick(Some(c as usize - '1' as usize));
        }
        _ => {}
    }
}

fn confirm_key(app: &mut App, key: KeyEvent) {
    // Esc backs out entirely rather than picking the second option: for an
    // overwrite prompt, "no" writes a different file and Esc writes nothing.
    if matches!(key.code, KeyCode::Esc) {
        app.close_overlay();
        return;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => answer(app, true),
        KeyCode::Char('n') | KeyCode::Char('N') => answer(app, false),
        _ => {}
    }
}

/// Yes or no to whatever is currently being asked.
fn answer(app: &mut App, yes: bool) {
    match &app.overlay {
        Overlay::Confirm(Confirm::Overwrite(path)) => {
            let path = path.clone();
            if yes {
                app.launch_merge(path);
            } else {
                app.merge_next_to(&path);
            }
        }
        Overlay::Confirm(Confirm::CancelMerge) | Overlay::Confirm(Confirm::CancelFetch) => {
            if yes {
                app.confirm_cancel();
            } else {
                app.close_overlay();
            }
        }
        _ => {}
    }
}

fn result_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter | KeyCode::Esc => app.dismiss_result(),
        KeyCode::Char(c) => result_command(app, c),
        _ => {}
    }
}

fn result_command(app: &mut App, c: char) {
    match c.to_ascii_lowercase() {
        'p' => app.open_output_folder(),
        'x' | 'q' => app.quit = true,
        _ => {}
    }
}
