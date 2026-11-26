//! Linux desktop-entry discovery and launch-scoped X11 window subscriptions.
use crate::{Result, appimage::AppImage, stream};
use freedesktop_desktop_entry as desktop;
use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xproto::{AtomEnum, ConnectionExt as _, MapState, WindowClass},
    },
    rust_connection::RustConnection,
};

struct App {
    id: String,
    name: String,
    path: PathBuf,
    exec: Option<String>,
    command: Option<Vec<String>>,
    startup_wm_class: Option<String>,
    dbus_activatable: bool,
    appimage: Option<AppImage>,
}

impl App {
    /// Recognize the stock browser command without discarding custom Exec arguments.
    fn chromium_command(&self) -> Option<&str> {
        if self.id != "chromium.desktop" {
            return None;
        }
        let mut args = self.exec.as_deref()?.split_whitespace();
        let cmd = args.next()?;
        if !matches!(cmd, "chromium" | "/usr/bin/chromium")
            || !args.all(|arg| matches!(arg, "%U" | "%u" | "%F" | "%f"))
        {
            return None;
        }
        Some(cmd)
    }

    fn webapp_command(&self) -> Option<(&str, &str)> {
        let mut args = self.exec.as_deref()?.split_whitespace();
        let cmd = args.next()?;
        let url = args.next()?;
        // Leave quoting, field codes and custom arguments to the desktop launcher.
        if !matches!(
            cmd,
            "omarchy-launch-webapp" | "/usr/bin/omarchy-launch-webapp"
        ) || args.next().is_some()
            || !url.starts_with("https://") && !url.starts_with("http://")
            || url.contains(['"', '\'', '\\', '%'])
        {
            return None;
        }
        Some((cmd, url))
    }

    fn chromium_profile(&self) -> Result<Option<PathBuf>> {
        let subdir = if self.chromium_command().is_some() {
            PathBuf::from("chromium")
        } else if self.webapp_command().is_some()
            && Command::new("omarchy-cmd-default-browser")
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|out| {
                    out.status.success() && out.stdout.trim_ascii() == b"chromium.desktop"
                })
        {
            PathBuf::from("webapps").join(&self.id)
        } else {
            return Ok(None);
        };
        let data = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
            .ok_or("Missing Chromium profile directory")?;
        Ok(Some(data.join("omarchy-launchpad").join(subdir)))
    }

    fn allows_window(
        &self,
        c: &RustConnection,
        window: u32,
        profile: Option<&Path>,
    ) -> Result<bool> {
        let Some(profile) = profile.filter(|_| self.webapp_command().is_some()) else {
            return Ok(true);
        };
        let class = c
            .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 4096)?
            .reply()?;
        // Chromium app windows use an app-specific instance. Normal browser
        // windows and browser popups use the profile's generic instance.
        let mut browser = b"chromium (".to_vec();
        browser.extend_from_slice(profile.as_os_str().as_bytes());
        browser.push(b')');
        Ok(class.value.split(|b| *b == 0).next() != Some(browser.as_slice()))
    }
}

fn chromium_owner(pid: u32, profile: &Path) -> bool {
    // Chromium rewrites argv and clears its original environment. The profile's
    // singleton lock identifies the browser process that owns this user-data dir.
    let Ok(owner) = fs::read_link(profile.join("SingletonLock")) else {
        return false;
    };
    let Ok(host) = fs::read_to_string("/proc/sys/kernel/hostname") else {
        return false;
    };
    owner.as_os_str() == OsStr::new(&format!("{}-{pid}", host.trim_end()))
        && fs::read_link(format!("/proc/{pid}/exe"))
            .is_ok_and(|path| path.file_name() == Some(OsStr::new("chromium")))
}

fn applications() -> Vec<App> {
    let locales = desktop::get_languages_from_env();
    let desktops = env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let visible_in = |names: Vec<&str>| {
        names
            .iter()
            .any(|name| desktops.split(':').any(|d| d.eq_ignore_ascii_case(name)))
    };
    let mut seen = BTreeSet::new();
    let mut apps = Vec::new();
    for base in desktop::default_paths() {
        for entry in desktop::Iter::new(std::iter::once(base.clone())).entries(Some(&locales)) {
            let Ok(relative) = entry
                .path
                .strip_prefix(base.canonicalize().unwrap_or_else(|_| base.clone()))
            else {
                continue;
            };
            let Some(id) = relative.to_str() else {
                continue;
            };
            let id = id.replace('/', "-");
            if !seen.insert(id.clone())
                || entry.type_() != Some("Application")
                || entry.hidden()
                || entry.no_display()
                || entry.terminal()
                || entry.only_show_in().is_some_and(|names| !visible_in(names))
                || entry.not_show_in().is_some_and(visible_in)
            {
                continue;
            }
            let Some(name) = entry.name(&locales) else {
                continue;
            };
            if [&id, name.as_ref()]
                .iter()
                .any(|s| s.is_empty() || s.len() > 4096 || s.chars().any(char::is_control))
            {
                continue;
            }
            let command = entry.parse_exec().ok();
            let appimage = command
                .as_deref()
                .and_then(|args| AppImage::from_exec(args, &id));
            apps.push(App {
                appimage,
                id,
                name: name.into_owned(),
                exec: entry.exec().map(str::to_owned),
                command,
                startup_wm_class: entry.startup_wm_class().map(str::to_owned),
                dbus_activatable: entry.dbus_activatable(),
                path: entry.path,
            });
        }
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}

// SSH login shells often lack the graphical session environment. Import only
// desktop settings from the user's systemd manager, preserving Xauthority.
fn session_environment() -> Result<()> {
    if env::var_os("DISPLAY").is_some_and(|s| !s.is_empty()) {
        return Ok(());
    }
    let output = Command::new("systemd-run")
        .args([
            "--user",
            "--quiet",
            "--pipe",
            "--wait",
            "/usr/bin/env",
            "-0",
        ])
        .stdin(Stdio::null())
        .output()?;
    let mut command = Command::new(env::current_exe()?);
    let mut display = false;
    for field in output.stdout.split(|b| *b == 0) {
        let Some(at) = field.iter().position(|b| *b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..at], &field[at + 1..]);
        if matches!(
            key,
            b"DISPLAY"
                | b"HYPRLAND_INSTANCE_SIGNATURE"
                | b"XAUTHORITY"
                | b"WAYLAND_DISPLAY"
                | b"XDG_CURRENT_DESKTOP"
                | b"XDG_DATA_DIRS"
                | b"XDG_DATA_HOME"
                | b"XDG_RUNTIME_DIR"
                | b"DBUS_SESSION_BUS_ADDRESS"
        ) {
            command.env(OsStr::from_bytes(key), OsStr::from_bytes(value));
            display |= key == b"DISPLAY" && !value.is_empty();
        }
    }
    if !output.status.success() || !display {
        return Err("No authenticated desktop DISPLAY. Import DISPLAY and XAUTHORITY into the graphical user's systemd environment, or configure the agent's login environment.".into());
    }
    Err(command.args(env::args_os().skip(1)).exec().into())
}

fn launch(app: &App, token: &str, profile: Option<&Path>) -> Result<()> {
    if let Some(profile) = profile {
        let mut data_dir = std::ffi::OsString::from("--user-data-dir=");
        data_dir.push(profile);
        let mut command = Command::new("setsid");
        command.arg("--wait");
        if let Some((cmd, url)) = app.webapp_command() {
            command.args([cmd, url]);
        } else {
            command.arg(app.chromium_command().ok_or("Invalid Chromium entry")?);
        }
        let mut child = command
            .arg("--ozone-platform=x11")
            .arg(data_dir)
            .env("X11_BRIDGE_LAUNCH_ID", token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    if app.dbus_activatable
        && let Some([program, args @ ..]) = app.command.as_deref()
    {
        // A session-bus activation can hand the request to an existing native
        // Wayland singleton, which neither inherits our launch token nor opens
        // an X11 window. Give the desktop command a private bus so its process
        // inherits both the token and the forced X11 backend.
        let mut child = graphical_command(token)
            .args(["dbus-run-session", "--"])
            .arg(program)
            .args(args)
            .spawn()?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    let status = graphical_command(token)
        .args(["gio", "launch"])
        .arg(&app.path)
        .status()?;
    if !status.success() {
        return Err("GIO could not launch the desktop entry.".into());
    }
    Ok(())
}

fn graphical_command(token: &str) -> Command {
    let mut command = Command::new("setsid");
    command
        .arg("--wait")
        .env("X11_BRIDGE_LAUNCH_ID", token)
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .env("SDL_VIDEODRIVER", "x11")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "x11")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn owner_pid(c: &RustConnection, window: u32) -> Result<Option<u32>> {
    // XRes identifies the actual local X client. A self-reported PID or
    // matching window title can identify an unrelated process.
    let reply = c
        .res_query_client_ids(&[ClientIdSpec {
            client: window,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        }])?
        .reply()?;
    Ok(reply.ids.iter().find_map(|id| id.value.first().copied()))
}

/// A tiled Xwayland client cannot grant its own ConfigureWindow request.
/// Match a managed top-level on both sides before changing the compositor layout.
pub(crate) fn resize_hyprland(
    c: &RustConnection,
    root: u32,
    window: u32,
    width: u32,
    height: u32,
) -> Result<bool> {
    let action = format!(
        r#"if not target.floating then
            hl.dispatch(hl.dsp.window.float({{window = target, action = "set"}}))
        end
        hl.dispatch(hl.dsp.window.resize({{window = target, x = {width}, y = {height}, relative = false}}))
        keep_on_monitor({width}, {height})"#
    );
    dispatch_hyprland(c, root, window, &action)
}

pub(crate) fn raise_hyprland(c: &RustConnection, root: u32, window: u32) -> Result<bool> {
    dispatch_hyprland(
        c,
        root,
        window,
        r#"fit_floating_window()
        hl.dispatch(hl.dsp.focus({window = target}))
        hl.dispatch(hl.dsp.window.alter_zorder({window = target, mode = "top"}))"#,
    )
}

fn dispatch_hyprland(c: &RustConnection, root: u32, window: u32, action: &str) -> Result<bool> {
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        return Ok(false);
    }
    let clients = c.intern_atom(true, b"_NET_CLIENT_LIST")?.reply()?.atom;
    if clients == 0 {
        return Ok(false);
    }
    let reply = c
        .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
        .reply()?;
    if reply.bytes_after != 0
        || !reply
            .value32()
            .is_some_and(|mut ids| ids.any(|id| id == window))
    {
        return Ok(false);
    }
    let Some(pid) = owner_pid(c, window)? else {
        return Ok(false);
    };
    let geometry = c.get_geometry(window)?.reply()?;
    let position = c.translate_coordinates(window, root, 0, 0)?.reply()?;

    // Match the window and act in one callback. The expression contains only
    // numeric X11 metadata and the action supplied by the caller above.
    let script = format!(
        r#"(function()
        local only, count, exact = nil, 0, nil
        for _, win in ipairs(hl.get_windows()) do
            if win.pid == {pid} and win.xwayland and win.mapped then
                only, count = win, count + 1
                if win.at and win.size
                    and math.floor(win.at.x) == {x} and math.floor(win.at.y) == {y}
                    and math.floor(win.size.x) == {w} and math.floor(win.size.y) == {h} then
                    assert(not exact, "ambiguous Xwayland geometry")
                    exact = win
                end
            end
        end
        local target = count == 1 and only or exact
        assert(target, "no unique Xwayland window")

        local function keep_on_monitor(width, height)
            local monitor = target.monitor
            if not monitor or not target.at then return end
            local monitor_width, monitor_height = monitor.width, monitor.height
            if monitor.transform % 2 == 1 then
                monitor_width, monitor_height = monitor_height, monitor_width
            end
            local right = monitor.x + math.floor(monitor_width / monitor.scale)
            local bottom = monitor.y + math.floor(monitor_height / monitor.scale)
            local x = math.max(monitor.x, math.min(target.at.x, right - width))
            local y = math.max(monitor.y, math.min(target.at.y, bottom - height))
            if x ~= target.at.x or y ~= target.at.y then
                hl.dispatch(hl.dsp.window.move({{window = target, x = math.floor(x), y = math.floor(y), relative = false}}))
            end
        end

        local function fit_floating_window()
            if target.floating and target.fullscreen == 0 and target.size then
                keep_on_monitor(target.size.x, target.size.y)
            end
        end

        {action}
    end)()"#,
        x = position.dst_x,
        y = position.dst_y,
        w = geometry.width,
        h = geometry.height,
    );
    let output = Command::new("hyprctl")
        .args(["eval", &script])
        .stdin(Stdio::null())
        .output();
    Ok(output.is_ok_and(|o| o.status.success() && o.stdout.trim_ascii() == b"ok"))
}

fn persistent_owner(pid: u32, profile: Option<&Path>, appimage: Option<&AppImage>) -> bool {
    profile.is_some_and(|dir| chromium_owner(pid, dir))
        || appimage.is_some_and(|image| image.owns(pid))
}

fn owns(
    c: &RustConnection,
    window: u32,
    token: &str,
    profile: Option<&Path>,
    appimage: Option<&AppImage>,
) -> Result<bool> {
    let marker = format!("X11_BRIDGE_LAUNCH_ID={token}");
    let Some(pid) = owner_pid(c, window)? else {
        return Ok(false);
    };
    Ok(fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|data| data.split(|b| *b == 0).any(|s| s == marker.as_bytes()))
        || persistent_owner(pid, profile, appimage))
}

fn installed_executable(app: &App) -> Option<PathBuf> {
    let program = app.command.as_deref()?.first()?;
    let path = Path::new(program);
    if path.is_absolute() {
        return path.canonicalize().ok();
    }
    if path.components().count() != 1 {
        return None;
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(path))
        .find_map(|candidate| candidate.canonicalize().ok())
}

fn class_matches(app: &App, class: &[u8]) -> bool {
    let matches = |candidate: &str| class.eq_ignore_ascii_case(candidate.as_bytes());
    app.startup_wm_class.as_deref().is_some_and(matches)
        || matches(app.id.trim_end_matches(".desktop"))
        || installed_executable(app)
            .and_then(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_owned))
            .is_some_and(|name| matches(&name))
}

// Only accept the desktop command itself or a sibling `.bin` implementation
// invoked by its installed wrapper script.
fn executable_matches(app: &App, running: &Path) -> bool {
    let Ok(running) = running.canonicalize() else {
        return false;
    };
    if app.id == "hermes-desktop.desktop" {
        let home = env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes")));
        if home.is_some_and(|home| {
            running.starts_with(home.join("hermes-agent/apps/desktop/release"))
                && running.file_name() == Some(OsStr::new("Hermes"))
        }) {
            return true;
        }
    }
    let Some(installed) = installed_executable(app) else {
        return false;
    };
    executable_paths_match(&app.id, &installed, &running)
}

fn executable_paths_match(id: &str, installed: &Path, running: &Path) -> bool {
    installed == running
        || (id == "chatgpt.desktop"
            && installed.file_name() == Some(OsStr::new("codex-launcher"))
            && installed.parent() == running.parent()
            && running.file_name() == Some(OsStr::new("ChatGPT")))
        || (installed.parent() == running.parent()
            && installed.file_name().is_some_and(|name| {
                let mut implementation = name.to_os_string();
                implementation.push(".bin");
                running.file_name() == Some(implementation.as_os_str())
            }))
}

fn existing_app_windows(
    c: &RustConnection,
    root: u32,
    app: &App,
    profile: Option<&Path>,
) -> Result<Vec<u32>> {
    let clients = c.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let list = c
        .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
        .reply()?;
    if list.bytes_after != 0 {
        return Ok(Vec::new());
    }
    let mut matched = Vec::new();
    for xid in list.value32().into_iter().flatten() {
        let Ok(attrs) = c.get_window_attributes(xid)?.reply() else {
            continue;
        };
        if attrs.map_state != MapState::VIEWABLE {
            continue;
        }
        if profile.is_some() || app.appimage.is_some() {
            if owner_pid(c, xid)?
                .is_some_and(|pid| persistent_owner(pid, profile, app.appimage.as_ref()))
                && app.allows_window(c, xid, profile)?
            {
                matched.push(xid);
            }
            continue;
        }
        let Ok(class) = c
            .get_property(false, xid, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 1024)?
            .reply()
        else {
            continue;
        };
        if !class
            .value
            .split(|b| *b == 0)
            .nth(1)
            .is_some_and(|name| class_matches(app, name))
        {
            continue;
        }
        let Some(pid) = owner_pid(c, xid)? else {
            continue;
        };
        let Ok(path) = fs::read_link(format!("/proc/{pid}/exe")) else {
            continue;
        };
        if executable_matches(app, &path) {
            matched.push(xid);
        }
    }
    Ok(matched)
}

fn app_by_id(id: &str) -> Result<App> {
    applications()
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| "Desktop entry is no longer available. Refresh the launcher.".into())
}

fn watch(
    c: &RustConnection,
    root: u32,
    token: &str,
    profile: Option<&Path>,
    app: Option<&App>,
    initial: &[u32],
) -> Result<()> {
    let appimage = app.and_then(|a| a.appimage.as_ref());
    let clients = c.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
    let existing_owners: BTreeSet<_> = initial
        .iter()
        .filter_map(|window| owner_pid(c, *window).ok().flatten())
        .collect();
    // Existing single-instance processes can own several unrelated documents.
    // Treat every current window from those processes as the baseline, then emit
    // only the selected existing window and windows that appear after this watch.
    let property = c
        .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
        .reply()?;
    let mut baseline: BTreeSet<_> = property.value32().into_iter().flatten().collect();
    baseline.extend(c.query_tree(root)?.reply()?.children);
    let mut known = BTreeSet::new();
    for window in baseline {
        let Ok(attrs) = c.get_window_attributes(window)?.reply() else {
            continue;
        };
        if attrs.class == WindowClass::INPUT_OUTPUT
            && attrs.map_state == MapState::VIEWABLE
            && owner_pid(c, window)?.is_some_and(|pid| existing_owners.contains(&pid))
        {
            known.insert(window);
        }
    }
    let mut empty_since = Instant::now();
    let mut appeared = !known.is_empty();
    eprintln!(
        "[x11-bridge-agent] watch start initial={known:?} owners={existing_owners:?} profile={} app={}",
        profile.is_some(),
        app.map_or("<any>", |app| app.id.as_str())
    );
    let closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = closed.clone();
    thread::spawn(move || {
        let _ = io::stdin().read(&mut [0]);
        stop.store(true, std::sync::atomic::Ordering::Release);
    });
    while !closed.load(std::sync::atomic::Ordering::Acquire) {
        let property = c
            .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
            .reply()?;
        if property.bytes_after != 0 {
            return Err("X11 window list too large".into());
        }
        let mut candidates: BTreeSet<_> = property.value32().into_iter().flatten().collect();
        // Root children include override-redirect menus and bare Xwayland clients.
        candidates.extend(c.query_tree(root)?.reply()?.children);
        let mut current = BTreeSet::new();
        for window in candidates {
            let Ok(attrs) = c.get_window_attributes(window)?.reply() else {
                continue;
            };
            if attrs.class != WindowClass::INPUT_OUTPUT
                || (!known.contains(&window) && attrs.map_state != MapState::VIEWABLE)
            {
                continue;
            }
            let pid = owner_pid(c, window)?;
            let token_owned = owns(c, window, token, profile, appimage)?;
            let existing_owned = pid.is_some_and(|pid| existing_owners.contains(&pid));
            let allowed = match app {
                Some(app) => app.allows_window(c, window, profile),
                None => Ok(true),
            }?;
            if (token_owned || existing_owned) && allowed {
                current.insert(window);
                if !known.contains(&window) {
                    eprintln!(
                        "[x11-bridge-agent] window added xid={window} pid={pid:?} token_owned={token_owned} existing_owned={existing_owned}"
                    );
                    println!("{window}");
                    io::stdout().flush()?;
                }
            }
        }
        for window in known.difference(&current) {
            eprintln!("[x11-bridge-agent] window removed xid={window}");
        }
        known = current;
        if known.is_empty() {
            if empty_since.elapsed() >= Duration::from_secs(if appeared { 3 } else { 20 }) {
                if appeared {
                    return Ok(());
                }
                return Err("No launch-owned X11 window appeared. Native Wayland, sandboxed and existing single-instance processes may not expose an attachable window.".into());
            }
        } else {
            appeared = true;
            empty_since = Instant::now();
        }
        thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<()> {
    session_environment()?;
    if args == ["list"] {
        for app in applications() {
            println!("{}\t{}", app.id, app.name)
        }
        return Ok(());
    }
    let [action, token, rest @ ..] = args else {
        return Err("expected desktop list, launch, watch or attach".into());
    };
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid launch identity".into());
    }
    let (c, screen) = x11rb::connect(None)?;
    let root = c.setup().roots[screen].root;
    let version = c.res_query_version(1, 2)?.reply()?;
    if (version.server_major, version.server_minor) < (1, 2) {
        return Err("XRes 1.2 is required to identify the launched process".into());
    }
    match (action.as_str(), rest) {
        ("open" | "launch", [id]) => {
            let app = app_by_id(id)?;
            let profile = app.chromium_profile()?;
            eprintln!(
                "[x11-bridge-agent] {action} app={} command={:?} startup_wm_class={:?} dbus={} profile={}",
                app.id,
                app.command,
                app.startup_wm_class,
                app.dbus_activatable,
                profile.is_some()
            );
            let existing = if action == "open" {
                existing_app_windows(&c, root, &app, profile.as_deref())?
            } else {
                Vec::new()
            };
            eprintln!("[x11-bridge-agent] existing windows={existing:?}");
            if existing.is_empty() {
                launch(&app, token, profile.as_deref())?;
            }
            let reconnected = !existing.is_empty() && profile.is_none() && app.appimage.is_none();
            if reconnected && let Some(xid) = existing.last() {
                eprintln!("[x11-bridge-agent] reconnect selected xid={xid}");
                println!("{xid}");
                io::stdout().flush()?;
            }
            watch(
                &c,
                root,
                token,
                profile.as_deref(),
                Some(&app),
                if reconnected { &existing } else { &[] },
            )
        }
        ("watch", []) => watch(&c, root, token, None, None, &[]),
        ("attach", [xid]) | ("attach-app", [xid, _]) => {
            let window = xid.parse()?;
            let mut nonce = [0; 8];
            fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
            // Subscribe before checking ownership. A later DestroyNotify ends
            // this generation even if the server reuses the resource number.
            c.change_window_attributes(
                window,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .event_mask(x11rb::protocol::xproto::EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
            let matches = if let [_, id] = rest {
                let app = app_by_id(id)?;
                let profile = app.chromium_profile()?;
                owns(&c, window, token, profile.as_deref(), app.appimage.as_ref())?
                    && app.allows_window(&c, window, profile.as_deref())?
                    || (profile.is_none()
                        && app.appimage.is_none()
                        && existing_app_windows(&c, root, &app, None)?.contains(&window))
            } else {
                owns(&c, window, token, None, None)?
            };
            eprintln!(
                "[x11-bridge-agent] attach xid={window} pid={:?} app={:?} matches={matches}",
                owner_pid(&c, window)?,
                rest.get(1)
            );
            if window == root || !matches {
                return Err("Window no longer belongs to this launch.".into());
            }
            dispatch_hyprland(&c, root, window, "fit_floating_window()")?;
            stream::run(
                &c,
                root,
                window,
                u64::from_ne_bytes(nonce).max(1),
                crate::input::InputMode::Desktop,
            )
        }
        _ => Err("invalid desktop operation".into()),
    }
}

#[cfg(test)]
mod executable_tests {
    use super::*;

    #[test]
    fn chatgpt_wrapper_matches_only_its_sibling_binary() {
        let wrapper = Path::new("/opt/chatgpt/codex-launcher");
        assert!(executable_paths_match(
            "chatgpt.desktop",
            wrapper,
            Path::new("/opt/chatgpt/ChatGPT")
        ));
        assert!(!executable_paths_match(
            "other.desktop",
            wrapper,
            Path::new("/opt/chatgpt/ChatGPT")
        ));
        assert!(!executable_paths_match(
            "chatgpt.desktop",
            wrapper,
            Path::new("/other/ChatGPT")
        ));
        assert!(!executable_paths_match(
            "chatgpt.desktop",
            wrapper,
            Path::new("/opt/chatgpt/other")
        ));
    }
}
