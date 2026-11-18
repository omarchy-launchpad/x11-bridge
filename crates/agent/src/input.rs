use crate::Result;
use std::collections::{BTreeMap, BTreeSet};
use x11_bridge_protocol::messages::Input;
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{xproto::*, xtest::ConnectionExt as _},
    rust_connection::RustConnection,
};

pub struct InputState<'a> {
    connection: &'a RustConnection,
    window: u32,
    root: u32,
    keys: BTreeMap<u32, u8>,
    buttons: BTreeSet<u8>,
}
impl<'a> InputState<'a> {
    pub fn new(connection: &'a RustConnection, window: u32, root: u32) -> Self {
        Self {
            connection,
            window,
            root,
            keys: BTreeMap::new(),
            buttons: BTreeSet::new(),
        }
    }
    pub fn focused(&self) -> Result<bool> {
        Ok(self.connection.get_input_focus()?.reply()?.focus == self.window)
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
        self.keys.clear();
        self.buttons.clear();
        self.connection.flush()?;
        Ok(())
    }
    pub fn apply(&mut self, input: Input) -> Result<()> {
        if input == Input::ReleaseAll {
            return self.release();
        }
        // Serialize focus validation and injection against other X clients.
        self.connection.grab_server()?.check()?;
        let result = self.apply_grabbed(input);
        let ungrab = self.connection.ungrab_server()?.check();
        self.connection.flush()?;
        ungrab?;
        result
    }
    fn apply_grabbed(&mut self, input: Input) -> Result<()> {
        if !self.focused()? {
            self.release()?;
            return Err("selected window does not have keyboard focus".into());
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
                if x >= u32::from(size.0) || y >= u32::from(size.1) {
                    return Err("pointer outside window".into());
                }
                let location = self
                    .connection
                    .translate_coordinates(
                        self.window,
                        self.root,
                        i16::try_from(x)?,
                        i16::try_from(y)?,
                    )?
                    .reply()?;
                if !location.same_screen {
                    return Err("pointer is on another screen".into());
                }
                self.connection
                    .xtest_fake_input(
                        MOTION_NOTIFY_EVENT,
                        0,
                        CURRENT_TIME,
                        self.root,
                        location.dst_x,
                        location.dst_y,
                        0,
                    )?
                    .check()?;
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
                if button > 5 {
                    return Err("only core buttons 1 through 5 are supported".into());
                }
                // XTEST buttons follow the pointer, not keyboard focus. Require
                // the pointer to be directly over the selected client or its child.
                let pointer = self.connection.query_pointer(self.window)?.reply()?;
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
                if !pressed {
                    self.buttons.remove(&button);
                }
            }
            Input::ReleaseAll => unreachable!(),
        }
        Ok(())
    }
}
impl Drop for InputState<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
