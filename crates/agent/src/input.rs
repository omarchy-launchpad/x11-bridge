use crate::Result;
use std::collections::{BTreeMap, BTreeSet};
use x11_bridge_protocol::messages::Input;
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{xproto::*, xtest::ConnectionExt as _},
    rust_connection::RustConnection,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Disabled,
    Focused,
    Desktop,
}

pub struct InputState<'a> {
    connection: &'a RustConnection,
    window: u32,
    root: u32,
    mode: InputMode,
    pointer_position: Option<(u32, u32)>,
    keys: BTreeMap<u32, u8>,
    buttons: BTreeSet<u8>,
}
impl<'a> InputState<'a> {
    pub fn new(connection: &'a RustConnection, window: u32, root: u32, mode: InputMode) -> Self {
        Self {
            connection,
            window,
            root,
            mode,
            pointer_position: None,
            keys: BTreeMap::new(),
            buttons: BTreeSet::new(),
        }
    }
    pub fn focused(&self) -> Result<bool> {
        Ok(self.connection.get_input_focus()?.reply()?.focus == self.window)
    }
    pub fn needs_focus(&self) -> bool {
        !self.keys.is_empty() || (self.mode == InputMode::Focused && !self.buttons.is_empty())
    }
    pub fn release(&mut self) -> Result<()> {
        // Only release input pressed by this transport.
        for key in self.keys.values().copied() {
            self.connection
                .xtest_fake_input(KEY_RELEASE_EVENT, key, CURRENT_TIME, self.root, 0, 0, 0)?
                .check()?;
        }
        for button in self.buttons.iter().copied() {
            self.connection
                .xtest_fake_input(
                    BUTTON_RELEASE_EVENT,
                    button,
                    CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                )?
                .check()?;
        }
        self.pointer_position = None;
        self.keys.clear();
        self.buttons.clear();
        self.connection.flush()?;
        Ok(())
    }
    pub fn apply(&mut self, input: Input) -> Result<()> {
        if input == Input::ReleaseAll {
            return self.release();
        }
        if matches!(
            input,
            Input::Button {
                button: 1..=3,
                pressed: true
            }
        ) && self.mode == InputMode::Desktop
            && self.buttons.is_empty()
            && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
            && let Some((x, y)) = self.pointer_position
        {
            self.raise_if_covered(x, y)?;
        }
        // Serialize focus validation and injection against other X clients.
        self.connection.grab_server()?.check()?;
        let result = self.apply_grabbed(input);
        let ungrab = self.connection.ungrab_server()?.check();
        self.connection.flush()?;
        ungrab?;
        result
    }

    fn raise_if_covered(&self, x: u32, y: u32) -> Result<()> {
        let geometry = self.connection.get_geometry(self.window)?.reply()?;
        if x < u32::from(geometry.width)
            && y < u32::from(geometry.height)
            && self
                .connection
                .get_window_attributes(self.window)?
                .reply()?
                .map_state
                == MapState::VIEWABLE
            && self.pointer_destination(x, y, true)?.is_none()
        {
            // The compositor needs the X server to apply the raise. Recheck
            // the target under the server grab before any input is injected.
            let raised = crate::desktop::raise_hyprland(self.connection, self.root, self.window)?;
            eprintln!(
                "[x11-bridge-input] raise xid={} at=({x},{y}) geometry={}x{} raised={raised}",
                self.window, geometry.width, geometry.height
            );
        }
        Ok(())
    }

    fn apply_grabbed(&mut self, input: Input) -> Result<()> {
        let focused = self.focused()?;
        let desktop_pointer =
            self.mode == InputMode::Desktop && !matches!(input, Input::Key { .. });
        if !focused && !desktop_pointer {
            return self.release();
        }
        if self
            .connection
            .get_window_attributes(self.window)?
            .reply()?
            .map_state
            != MapState::VIEWABLE
        {
            self.release()?;
            return Err("selected window is not viewable".into());
        }
        let geometry = self.connection.get_geometry(self.window)?.reply()?;
        let size = (geometry.width, geometry.height);
        match input {
            Input::Motion { x, y } => {
                self.pointer_position = Some((x, y));
                if !focused && self.buttons.is_empty() {
                    return Ok(());
                }
                if !self.move_pointer(x, y, size, desktop_pointer)? && !desktop_pointer {
                    return Err("pointer outside window".into());
                }
            }
            Input::Key { keysym, pressed } => {
                // Level-zero keysyms only. Modifiers are explicit key presses.
                let key = if let Some(key) = self.keys.get(&keysym) {
                    *key
                } else {
                    if !pressed {
                        return Ok(());
                    }
                    let setup = self.connection.setup();
                    let count = setup
                        .max_keycode
                        .checked_sub(setup.min_keycode)
                        .and_then(|n| n.checked_add(1))
                        .ok_or("invalid keyboard range")?;
                    let mapping = self
                        .connection
                        .get_keyboard_mapping(setup.min_keycode, count)?
                        .reply()?;
                    let per = usize::from(mapping.keysyms_per_keycode);
                    if per == 0 || keysym == 0 {
                        return Err("unsupported keysym".into());
                    }
                    let offset = mapping
                        .keysyms
                        .chunks(per)
                        .position(|symbols| symbols[0] == keysym)
                        .ok_or("keysym has no level-zero keycode")?;
                    let key = setup
                        .min_keycode
                        .checked_add(u8::try_from(offset)?)
                        .ok_or("invalid keycode")?;
                    let state = self.connection.query_keymap()?.reply()?;
                    if state.keys[usize::from(key / 8)] & (1 << (key % 8)) != 0 {
                        return Err("key already held outside this transport".into());
                    }
                    self.keys.insert(keysym, key);
                    key
                };
                self.connection
                    .xtest_fake_input(
                        if pressed {
                            KEY_PRESS_EVENT
                        } else {
                            KEY_RELEASE_EVENT
                        },
                        key,
                        CURRENT_TIME,
                        self.root,
                        0,
                        0,
                        0,
                    )?
                    .check()?;
                if !pressed {
                    self.keys.remove(&keysym);
                }
            }
            Input::Button { button, pressed } => {
                eprintln!(
                    "[x11-bridge-input] button xid={} button={button} pressed={pressed} focused={focused} position={:?} geometry={}x{}",
                    self.window, self.pointer_position, size.0, size.1
                );
                if button > 5 {
                    return Err("only core buttons 1 through 5 are supported".into());
                }
                // The activating click includes its own motion packet. An unfocused
                // desktop viewer defers that motion until the user presses a button.
                let destination = if desktop_pointer {
                    let Some((x, y)) = self.pointer_position else {
                        return self.release();
                    };
                    if !self.move_pointer(x, y, size, true)? {
                        return self.release();
                    }
                    Some((x, y))
                } else {
                    None
                };
                // XTEST buttons follow the pointer, not keyboard focus. Require
                // the pointer to be directly over the selected client or its child.
                let pointer = self.connection.query_pointer(self.window)?.reply()?;
                if destination.is_some_and(|(x, y)| {
                    u32::try_from(pointer.win_x).ok() != Some(x)
                        || u32::try_from(pointer.win_y).ok() != Some(y)
                }) {
                    // XTEST may clamp the pointer to another pixel of this window.
                    return self.release();
                }
                if !pointer.same_screen
                    || pointer.win_x < 0
                    || pointer.win_y < 0
                    || pointer.win_x as u16 >= size.0
                    || pointer.win_y as u16 >= size.1
                {
                    return Err("pointer outside selected window".into());
                }
                let root_pointer = self.connection.query_pointer(self.root)?.reply()?;
                let mut child = root_pointer.child;
                let mut target = false;
                for _ in 0..64 {
                    if child == self.window {
                        target = true;
                        break;
                    }
                    if child == 0 {
                        break;
                    }
                    child = self.connection.query_pointer(child)?.reply()?.child;
                }
                if !target {
                    eprintln!(
                        "[x11-bridge-input] blocked xid={} reason=obscured root_child={} local_child={} root=({},{}) local=({},{})",
                        self.window,
                        root_pointer.child,
                        pointer.child,
                        root_pointer.root_x,
                        root_pointer.root_y,
                        pointer.win_x,
                        pointer.win_y
                    );
                    return Err("selected window is obscured at pointer".into());
                }
                if pressed && !self.buttons.contains(&button) {
                    if button <= 5 && u16::from(pointer.mask) & (1 << (7 + button)) != 0 {
                        return Err("button already held outside this transport".into());
                    }
                    self.buttons.insert(button);
                } else if !pressed && !self.buttons.contains(&button) {
                    return Ok(());
                }
                self.connection
                    .xtest_fake_input(
                        if pressed {
                            BUTTON_PRESS_EVENT
                        } else {
                            BUTTON_RELEASE_EVENT
                        },
                        button,
                        CURRENT_TIME,
                        self.root,
                        0,
                        0,
                        0,
                    )?
                    .check()?;
                eprintln!(
                    "[x11-bridge-input] delivered xid={} button={button} pressed={pressed} root=({},{}) local=({},{}) child={}",
                    self.window,
                    root_pointer.root_x,
                    root_pointer.root_y,
                    pointer.win_x,
                    pointer.win_y,
                    pointer.child
                );
                if !pressed {
                    self.buttons.remove(&button);
                }
            }
            Input::ReleaseAll => unreachable!(),
        }
        Ok(())
    }
    fn move_pointer(&self, x: u32, y: u32, size: (u16, u16), check_target: bool) -> Result<bool> {
        if x >= u32::from(size.0) || y >= u32::from(size.1) {
            return Ok(false);
        }
        let Some((root_x, root_y)) = self.pointer_destination(x, y, check_target)? else {
            return Ok(false);
        };
        self.connection
            .xtest_fake_input(
                MOTION_NOTIFY_EVENT,
                0,
                CURRENT_TIME,
                self.root,
                root_x,
                root_y,
                0,
            )?
            .check()?;
        Ok(true)
    }

    fn pointer_destination(
        &self,
        x: u32,
        y: u32,
        check_target: bool,
    ) -> Result<Option<(i16, i16)>> {
        let x = i16::try_from(x)?;
        let y = i16::try_from(y)?;
        let location = self
            .connection
            .translate_coordinates(self.window, self.root, x, y)?
            .reply()?;
        if !location.same_screen {
            return Ok(None);
        }
        if check_target {
            let bounds = self.connection.get_geometry(self.root)?.reply()?;
            if location.dst_x < 0
                || location.dst_y < 0
                || location.dst_x as u16 >= bounds.width
                || location.dst_y as u16 >= bounds.height
            {
                return Ok(None);
            }
            // Walk the stacking order at the requested point before moving the
            // global pointer. A reparenting window manager may own the root child.
            let mut child = location.child;
            for _ in 0..64 {
                if child == self.window {
                    break;
                }
                if child == 0 {
                    return Ok(None);
                }
                child = self
                    .connection
                    .translate_coordinates(self.window, child, x, y)?
                    .reply()?
                    .child;
            }
            if child != self.window {
                return Ok(None);
            }
        }
        Ok(Some((location.dst_x, location.dst_y)))
    }
}
impl Drop for InputState<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
