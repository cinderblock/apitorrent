//! Finding the Deluge installations on this machine, so the import wizard can list them.
//!
//! Three places say where a Deluge config directory is:
//!
//! - **Running daemons** (Linux, `/proc`): `deluged`'s `-c`/`--config` argument, or its
//!   user's `~/.config/deluge` when it has none.
//! - **systemd units** that start `deluged`, the same way. These find a daemon that is
//!   stopped too.
//! - **The usual places**: Debian's package (`/var/lib/deluged/config`), each home
//!   directory's `.config/deluge`, and on Windows `%APPDATA%\deluge`.
//!
//! A candidate counts only if it holds Deluge's `core.conf` or `state` directory. One that
//! is there but can't be read (another user's, mode 0700) is still listed, with why: the
//! fix is usually to run apitorrent as Deluge's user.
//!
//! Nothing here writes anything, starts anything or talks to a daemon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A Deluge installation found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The config directory, canonical when it could be resolved.
    pub config_dir: PathBuf,
    /// A short name for it: the systemd unit (`deluged`), else the user whose home it is
    /// in, else the directory's name.
    pub name: String,
    /// A `deluged` process is running with this config directory.
    pub running: bool,
    /// The systemd units that start it.
    pub units: Vec<String>,
    /// The daemon's RPC port, from `core.conf`.
    pub daemon_port: Option<u16>,
    /// Its torrents (`state/*.torrent`), when the directory could be read.
    pub torrents: Option<u32>,
    /// Why it couldn't be read, if it couldn't.
    pub problem: Option<String>,
}

/// Where to look: the real machine, or a fake one in tests.
#[derive(Debug, Clone)]
pub struct Places {
    /// `/proc`, or `None` where there is none.
    pub proc: Option<PathBuf>,
    /// `/etc/passwd`: users' home directories, by uid and by name.
    pub passwd: Option<PathBuf>,
    /// Directories holding systemd unit files.
    pub unit_dirs: Vec<PathBuf>,
    /// Directories holding home directories (`/home`).
    pub home_parents: Vec<PathBuf>,
    /// Config directories to try as they are.
    pub candidates: Vec<PathBuf>,
}

impl Places {
    /// This machine's.
    #[must_use]
    pub fn system() -> Self {
        let mut candidates = vec![
            PathBuf::from("/var/lib/deluged/config"),
            PathBuf::from("/var/lib/deluged"),
            PathBuf::from("/var/lib/deluge/.config/deluge"),
            PathBuf::from("/root/.config/deluge"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(".config/deluge"));
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            candidates.push(PathBuf::from(appdata).join("deluge"));
        }
        let unix = cfg!(unix);
        Self {
            proc: (cfg!(target_os = "linux")).then(|| PathBuf::from("/proc")),
            passwd: unix.then(|| PathBuf::from("/etc/passwd")),
            unit_dirs: if cfg!(target_os = "linux") {
                [
                    "/etc/systemd/system",
                    "/usr/lib/systemd/system",
                    "/lib/systemd/system",
                ]
                .into_iter()
                .map(PathBuf::from)
                .collect()
            } else {
                Vec::new()
            },
            home_parents: if unix {
                vec![PathBuf::from("/home")]
            } else {
                Vec::new()
            },
            candidates,
        }
    }
}

/// The Deluge installations in `places`, running ones first, then by name.
#[must_use]
pub fn discover(places: &Places) -> Vec<Found> {
    let users = places
        .passwd
        .as_deref()
        .map(read_passwd)
        .unwrap_or_default();
    // Each candidate, with what it learned from where it was found.
    let mut seen: Vec<(PathBuf, Hint)> = Vec::new();

    if let Some(proc) = &places.proc {
        for (dir, user, _) in running_daemons(proc, &users) {
            seen.push((
                dir,
                Hint {
                    running: true,
                    user,
                    unit: None,
                },
            ));
        }
    }
    for dir in &places.unit_dirs {
        for (unit, args, user) in deluged_units(dir) {
            let home = users.by_name(user.as_deref().unwrap_or("root"));
            if let Some(config) = config_dir_of(&args, home, None) {
                seen.push((
                    config,
                    Hint {
                        running: false,
                        user,
                        unit: Some(unit),
                    },
                ));
            }
        }
    }
    for parent in &places.home_parents {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for e in entries.flatten() {
                let user = e.file_name().to_string_lossy().into_owned();
                seen.push((
                    e.path().join(".config/deluge"),
                    Hint {
                        user: Some(user),
                        ..Hint::default()
                    },
                ));
            }
        }
    }
    for c in &places.candidates {
        seen.push((c.clone(), Hint::default()));
    }

    let mut found: BTreeMap<PathBuf, Found> = BTreeMap::new();
    for (dir, hint) in seen {
        let inspected = inspect(&dir).or_else(|| {
            // A daemon's or a unit's directory that we can't even reach is still worth
            // listing: it is Deluge's, and the reason is the fix.
            let named = hint.running || hint.unit.is_some();
            let denied = std::fs::metadata(&dir)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
            (named && denied).then(|| blank(&dir, Some(unreadable())))
        });
        let Some(mut f) = inspected else { continue };
        let key = f.config_dir.clone();
        if let Some(existing) = found.get_mut(&key) {
            existing.running |= hint.running;
            if let Some(u) = hint.unit
                && !existing.units.contains(&u)
            {
                existing.units.push(u);
            }
            continue;
        }
        f.running = hint.running;
        f.units.extend(hint.unit);
        f.name = hint.user.unwrap_or_default();
        found.insert(key, f);
    }
    let mut out: Vec<Found> = found
        .into_values()
        .map(|mut f| {
            f.name = name_for(&f);
            f
        })
        .collect();
    out.sort_by(|a, b| b.running.cmp(&a.running).then_with(|| a.name.cmp(&b.name)));
    out
}

#[derive(Debug, Default)]
struct Hint {
    running: bool,
    user: Option<String>,
    unit: Option<String>,
}

/// The unit's name, else the user's, else the directory's (skipping `config`/`deluge`).
fn name_for(f: &Found) -> String {
    if let Some(u) = f.units.first() {
        return u.trim_end_matches(".service").to_owned();
    }
    if !f.name.is_empty() && f.name != "root" {
        return f.name.clone();
    }
    f.config_dir
        .ancestors()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .find(|n| !matches!(n.as_str(), "config" | ".config" | "deluge"))
        .unwrap_or_else(|| "deluge".to_owned())
}

/// A Deluge config directory, or `None` if `dir` isn't one (or doesn't exist).
fn inspect(dir: &Path) -> Option<Found> {
    let core = dir.join("core.conf");
    let state = dir.join("state");
    let blank = |problem| blank(dir, problem);
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        _ => return None,
    }
    // A directory we can't list: Deluge's or not, we can't tell, unless it is named like it.
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let looks = dir.ends_with(".config/deluge")
                || dir.ends_with("deluged")
                || dir.ends_with("deluged/config");
            return looks.then(|| blank(Some(unreadable())));
        }
        Err(_) => return None,
    };
    drop(entries);
    let has_core = core.is_file();
    if !has_core && !state.is_dir() {
        return None;
    }
    let mut f = blank(None);
    match crate::read_config(&core) {
        Ok(c) => {
            f.daemon_port = c
                .get("daemon_port")
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u16::try_from(p).ok());
        }
        Err(e) if has_core && e.kind() == std::io::ErrorKind::PermissionDenied => {
            f.problem = Some(unreadable());
        }
        Err(_) => {}
    }
    match std::fs::read_dir(&state) {
        Ok(entries) => {
            let n = entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "torrent"))
                .count();
            f.torrents = Some(u32::try_from(n).unwrap_or(u32::MAX));
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            f.problem = Some(unreadable());
        }
        Err(_) => {}
    }
    Some(f)
}

fn blank(dir: &Path, problem: Option<String>) -> Found {
    Found {
        config_dir: std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
        name: String::new(),
        running: false,
        units: Vec::new(),
        daemon_port: None,
        torrents: None,
        problem,
    }
}

fn unreadable() -> String {
    "apitorrent can't read it: run apitorrent as the user Deluge runs as".to_owned()
}

/// Users from `/etc/passwd`: uid and name to home directory.
#[derive(Debug, Default)]
struct Users {
    by_uid: BTreeMap<u32, (String, PathBuf)>,
}

impl Users {
    fn by_name(&self, name: &str) -> Option<&Path> {
        self.by_uid
            .values()
            .find(|(n, _)| n == name)
            .map(|(_, h)| h.as_path())
    }
}

fn read_passwd(path: &Path) -> Users {
    let mut users = Users::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        return users;
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 6
            && let Ok(uid) = f[2].parse::<u32>()
        {
            users
                .by_uid
                .insert(uid, (f[0].to_owned(), PathBuf::from(f[5])));
        }
    }
    users
}

/// A `deluged` process running with a given config directory, and where its command line
/// told it to listen for clients.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Running {
    /// `-p`/`--port`: overrides `core.conf`'s `daemon_port`.
    pub port: Option<u16>,
    /// `-u`/`--ui-interface`: the address its RPC listens on.
    pub interface: Option<String>,
}

/// The `deluged` process running with `config_dir` as its config directory, if any (Linux
/// only: elsewhere there is no `/proc` to look in, and this is `None`).
#[must_use]
pub fn running_daemon(places: &Places, config_dir: &Path) -> Option<Running> {
    let proc = places.proc.as_deref()?;
    let users = places
        .passwd
        .as_deref()
        .map(read_passwd)
        .unwrap_or_default();
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let want = canonical(config_dir);
    running_daemons(proc, &users)
        .into_iter()
        .find(|(dir, _, _)| canonical(dir) == want)
        .map(|(_, _, args)| Running {
            port: option_value(&args, "-p", "--port").and_then(|p| p.parse().ok()),
            interface: option_value(&args, "-u", "--ui-interface").map(str::to_owned),
        })
}

/// Each running `deluged`'s config directory, the user it runs as, and its arguments.
fn running_daemons(proc: &Path, users: &Users) -> Vec<(PathBuf, Option<String>, Vec<String>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc) else {
        return out;
    };
    for e in entries.flatten() {
        let pid = e.path();
        if !e
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(raw) = std::fs::read(pid.join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        let Some(rest) = after_deluged(&args) else {
            continue;
        };
        let uid = std::fs::read_to_string(pid.join("status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|u| u.parse::<u32>().ok())
            });
        let user = uid.and_then(|u| users.by_uid.get(&u));
        let cwd = std::fs::read_link(pid.join("cwd")).ok();
        if let Some(dir) = config_dir_of(rest, user.map(|(_, h)| h.as_path()), cwd.as_deref()) {
            out.push((dir, user.map(|(n, _)| n.clone()), rest.to_vec()));
        }
    }
    out
}

/// The arguments after the `deluged` program in a command line (`deluged -c x`, or
/// `python3 /usr/bin/deluged -c x`), or `None` if it doesn't run `deluged`.
fn after_deluged(args: &[String]) -> Option<&[String]> {
    let is_deluged = |a: &String| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n == "deluged" || n == "deluged.exe" || n == "deluged-script.py")
    };
    // The program itself, or the script an interpreter runs (within its first two args).
    let i = args.iter().take(3).position(is_deluged)?;
    Some(&args[i + 1..])
}

/// The config directory a `deluged` command line uses: `-c X`, `-cX`, `--config X`,
/// `--config=X`; else `~/.config/deluge`.
fn config_dir_of(args: &[String], home: Option<&Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    match option_value(args, "-c", "--config") {
        Some(g) => {
            let g = expand_home(g, home);
            if g.is_absolute() {
                Some(g)
            } else {
                cwd.map(|c| c.join(g))
            }
        }
        None => home.map(|h| h.join(".config/deluge")),
    }
}

/// An option's value in a command line: `-x V`, `-xV`, `--long V` or `--long=V`. The last
/// one wins, as with Deluge's own parser.
fn option_value<'a>(args: &'a [String], short: &str, long: &str) -> Option<&'a str> {
    let mut it = args.iter();
    let mut given = None;
    while let Some(a) = it.next() {
        if a == short || a == long {
            given = it.next().map(String::as_str);
        } else if let Some(v) = a.strip_prefix(long).and_then(|v| v.strip_prefix('=')) {
            given = Some(v);
        } else if let Some(v) = a.strip_prefix(short)
            && !v.is_empty()
            && !a.starts_with("--")
        {
            given = Some(v);
        }
    }
    given
}

fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(path),
    }
}

/// systemd units that start `deluged`: name, the arguments after it, and `User=`.
fn deluged_units(dir: &Path) -> Vec<(String, Vec<String>, Option<String>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".service") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let mut exec = None;
        let mut user = None;
        for line in text.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("ExecStart=") {
                // `-`, `@`, `+`, `!` prefixes change how systemd runs it, not what.
                exec = Some(v.trim_start_matches(['-', '@', '+', '!']).to_owned());
            } else if let Some(v) = line.strip_prefix("User=") {
                user = Some(v.trim().to_owned());
            }
        }
        let Some(exec) = exec else { continue };
        let args: Vec<String> = exec
            .split_whitespace()
            .map(|a| a.trim_matches('"').to_owned())
            .collect();
        if let Some(rest) = after_deluged(&args) {
            out.push((name, rest.to_vec(), user));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deluge_dir(at: &Path, torrents: usize, port: u16) {
        std::fs::create_dir_all(at.join("state")).unwrap();
        std::fs::write(
            at.join("core.conf"),
            format!("{{\"file\": 1, \"format\": 1}}{{\"daemon_port\": {port}}}"),
        )
        .unwrap();
        for i in 0..torrents {
            std::fs::write(
                at.join(format!("state/{i:040}.torrent")),
                b"d4:infod4:name1:xee",
            )
            .unwrap();
        }
    }

    fn places(root: &Path) -> Places {
        Places {
            proc: Some(root.join("proc")),
            passwd: Some(root.join("etc/passwd")),
            unit_dirs: vec![root.join("etc/systemd/system")],
            home_parents: vec![root.join("home")],
            candidates: vec![root.join("var/lib/deluged/config"), root.join("nowhere")],
        }
    }

    fn process(root: &Path, pid: u32, uid: u32, args: &[&str]) {
        let p = root.join(format!("proc/{pid}"));
        std::fs::create_dir_all(&p).unwrap();
        let mut raw = args.join("\0").into_bytes();
        raw.push(0);
        std::fs::write(p.join("cmdline"), raw).unwrap();
        std::fs::write(
            p.join("status"),
            format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
    }

    /// pokol's layout: two daemons under one package user, each with `--config`, each
    /// with its own unit; one more in a home directory, not running.
    // The fake machine is Linux: its `/proc`, `/etc/passwd` and units hold Linux paths,
    // which only parse as such on Unix. Elsewhere `Places::system()` leaves them out.
    #[cfg(unix)]
    #[test]
    fn finds_daemons_units_and_homes() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let lib = r.join("var/lib");
        deluge_dir(&lib.join("deluged"), 3, 58846);
        deluge_dir(&lib.join("delugex"), 2, 58847);
        deluge_dir(&r.join("home/alice/.config/deluge"), 1, 58848);
        std::fs::create_dir_all(r.join("home/bob")).unwrap();
        std::fs::create_dir_all(r.join("etc/systemd/system")).unwrap();
        std::fs::write(
            r.join("etc/passwd"),
            format!(
                "root:x:0:0::/root:/bin/sh\ndebian-deluged:x:108:113::{}:/usr/sbin/nologin\nalice:x:1000:1000::{}:/bin/sh\n",
                lib.join("deluged").display(),
                r.join("home/alice").display()
            ),
        )
        .unwrap();
        let d = lib.join("deluged").display().to_string();
        let x = lib.join("delugex").display().to_string();
        process(
            r,
            100,
            108,
            &[
                "/usr/bin/python3",
                "/usr/bin/deluged",
                "--do-not-daemonize",
                "--config",
                &d,
            ],
        );
        process(
            r,
            101,
            108,
            &[
                "/usr/bin/python3",
                "/usr/bin/deluged",
                &format!("--config={x}"),
            ],
        );
        process(r, 102, 1000, &["/usr/bin/python3", "/usr/bin/deluge-web"]);
        std::fs::write(
            r.join("etc/systemd/system/delugex.service"),
            format!("[Service]\nUser=debian-deluged\nExecStart=/usr/bin/deluged --do-not-daemonize --config {x} --ui-interface ::\n"),
        )
        .unwrap();
        std::fs::write(
            r.join("etc/systemd/system/other.service"),
            "[Service]\nExecStart=/bin/true\n",
        )
        .unwrap();

        let found = discover(&places(r));
        let summary: Vec<(String, bool, Option<u32>, Option<u16>)> = found
            .iter()
            .map(|f| (f.name.clone(), f.running, f.torrents, f.daemon_port))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("debian-deluged".to_owned(), true, Some(3), Some(58846)),
                ("delugex".to_owned(), true, Some(2), Some(58847)),
                ("alice".to_owned(), false, Some(1), Some(58848)),
            ]
        );
        assert_eq!(found[1].units, vec!["delugex.service".to_owned()]);
        assert!(found.iter().all(|f| f.problem.is_none()));
    }

    #[test]
    fn a_running_daemon_says_where_it_listens() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let d = r.join("var/lib/deluged");
        let x = r.join("var/lib/delugex");
        deluge_dir(&d, 0, 58846);
        deluge_dir(&x, 0, 58847);
        std::fs::create_dir_all(r.join("etc")).unwrap();
        std::fs::write(r.join("etc/passwd"), "").unwrap();
        let ds = d.display().to_string();
        process(r, 5, 0, &["/usr/bin/deluged", "-d", "-c", &ds]);
        let xs = format!("--config={}", x.display());
        process(
            r,
            6,
            0,
            &["/usr/bin/deluged", &xs, "--ui-interface", "::", "-p58900"],
        );
        let p = places(r);
        assert_eq!(running_daemon(&p, &d), Some(Running::default()));
        assert_eq!(
            running_daemon(&p, &x),
            Some(Running {
                port: Some(58900),
                interface: Some("::".to_owned()),
            })
        );
        assert_eq!(
            running_daemon(&p, &r.join("home/alice/.config/deluge")),
            None
        );
    }

    // The fake machine is Linux: its `/proc`, `/etc/passwd` and units hold Linux paths,
    // which only parse as such on Unix. Elsewhere `Places::system()` leaves them out.
    #[cfg(unix)]
    #[test]
    fn a_daemon_without_config_uses_its_users_home() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        deluge_dir(&r.join("home/alice/.config/deluge"), 0, 58846);
        std::fs::create_dir_all(r.join("etc")).unwrap();
        std::fs::write(
            r.join("etc/passwd"),
            format!(
                "alice:x:1000:1000::{}:/bin/sh\n",
                r.join("home/alice").display()
            ),
        )
        .unwrap();
        process(r, 7, 1000, &["deluged", "-d"]);
        let mut p = places(r);
        p.home_parents.clear();
        let found = discover(&p);
        assert_eq!(found.len(), 1);
        assert!(found[0].running);
        assert_eq!(found[0].name, "alice");
        assert_eq!(found[0].torrents, Some(0));
    }

    // The fake machine is Linux: its `/proc`, `/etc/passwd` and units hold Linux paths,
    // which only parse as such on Unix. Elsewhere `Places::system()` leaves them out.
    #[cfg(unix)]
    #[test]
    fn config_arguments() {
        let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let home = Path::new("/home/u");
        assert_eq!(
            config_dir_of(&a(&["-c", "/x"]), Some(home), None),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            config_dir_of(&a(&["-c/x"]), Some(home), None),
            Some(PathBuf::from("/x"))
        );
        assert_eq!(
            config_dir_of(&a(&["--config=~/d"]), Some(home), None),
            Some(PathBuf::from("/home/u/d"))
        );
        assert_eq!(
            config_dir_of(&a(&["-c", "rel"]), None, Some(Path::new("/w"))),
            Some(PathBuf::from("/w/rel"))
        );
        assert_eq!(
            config_dir_of(&a(&["-d"]), Some(home), None),
            Some(PathBuf::from("/home/u/.config/deluge"))
        );
        assert_eq!(config_dir_of(&a(&["-d"]), None, None), None);
        assert!(after_deluged(&a(&["/usr/bin/python3", "/usr/bin/deluge-web"])).is_none());
        assert_eq!(after_deluged(&a(&["deluged", "-d"])), Some(&a(&["-d"])[..]));
    }

    #[test]
    fn a_folder_without_deluge_files_is_not_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        std::fs::create_dir_all(r.join("var/lib/deluged/config/other")).unwrap();
        assert!(discover(&places(r)).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_one_says_why() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let dir = r.join("var/lib/deluged/config");
        deluge_dir(&dir, 1, 58846);
        std::fs::set_permissions(dir.join("state"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let found = discover(&places(r));
        std::fs::set_permissions(dir.join("state"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        // Root reads anything: then there's no problem to report.
        if std::fs::read_dir("/root").is_ok() {
            return;
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].torrents, None);
        assert!(found[0].problem.is_some());
    }

    /// Windows has no `/proc`, `/etc/passwd`, systemd units or `/home`: only the usual
    /// places, `%APPDATA%\deluge` among them.
    #[cfg(windows)]
    #[test]
    fn windows_looks_only_in_the_usual_places() {
        let p = Places::system();
        assert_eq!(p.proc, None);
        assert_eq!(p.passwd, None);
        assert!(p.unit_dirs.is_empty());
        assert!(p.home_parents.is_empty());
        let appdata = std::env::var_os("APPDATA").expect("APPDATA is set on Windows");
        let deluge = PathBuf::from(appdata).join("deluge");
        assert!(p.candidates.contains(&deluge));
        assert_eq!(running_daemon(&p, Path::new(r"C:\x")), None);
    }
}
