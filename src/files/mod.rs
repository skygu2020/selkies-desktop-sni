//! The file manager's model: the folders its windows show, how they are
//! listed and sorted, the places in the sidebar, the clipboard shared by
//! every window, the thumbnails, the watch that refreshes a folder when it
//! changes, and the background workers.

pub mod mime;
pub mod ops;

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::config;
use crate::desktop::Geometry;
use crate::i18n::{tr, trf};

pub use mime::MimeDb;
pub use ops::{Message, ThumbKey, Worker};

/// How long after the last change in a folder its listing is read again.
const RESCAN_SETTLE: Duration = Duration::from_millis(250);
/// How long a status message stays before the item count returns.
pub const STATUS_LINGER: Duration = Duration::from_secs(4);
/// Default window width.
pub const DEFAULT_WIDTH: i32 = 860;
/// Default window height.
pub const DEFAULT_HEIGHT: i32 = 560;

/// One file or folder in a listing.
#[derive(Clone, Debug)]
pub struct Item {
    /// File name.
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// Whether the item is a directory, following symlinks.
    pub is_dir: bool,
    /// Whether the item is a symlink.
    pub is_symlink: bool,
    /// Size in bytes, zero for directories.
    pub size: u64,
    /// Modification time, seconds since the epoch.
    pub modified: i64,
    /// Mime type.
    pub mime: String,
    /// Whether the name marks the item hidden.
    pub hidden: bool,
    /// Whether the file may be executed.
    pub executable: bool,
}

/// What a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// By name, folders first.
    Name,
    /// By size.
    Size,
    /// By modification time.
    Modified,
    /// By type.
    Kind,
}

impl SortKey {
    /// The column label.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => tr("Name"),
            SortKey::Size => tr("Size"),
            SortKey::Modified => tr("Modified"),
            SortKey::Kind => tr("Type"),
        }
    }

    /// The "sort by" menu text for the key.
    pub fn sort_label(self) -> &'static str {
        match self {
            SortKey::Name => tr("Sort by name"),
            SortKey::Size => tr("Sort by size"),
            SortKey::Modified => tr("Sort by date"),
            SortKey::Kind => tr("Sort by type"),
        }
    }
}

/// How a listing is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// A grid of large icons.
    Icons,
    /// Rows with columns.
    List,
}

/// A quick link in the sidebar.
#[derive(Clone, Debug)]
pub struct Place {
    /// Display name.
    pub name: String,
    /// The directory.
    pub path: PathBuf,
    /// Icon name.
    pub icon: &'static str,
}

/// What a picker window is choosing for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickTarget {
    /// A wallpaper image, written to a config key.
    Wallpaper { key: &'static str, monitor: Option<u32> },
    /// The start button's icon.
    StartIcon,
}

/// The settings new windows start with.
#[derive(Clone, Copy, Debug)]
pub struct Defaults {
    /// The view.
    pub view: View,
    /// The sort key.
    pub sort: SortKey,
    /// Whether hidden files are shown.
    pub show_hidden: bool,
}

/// A window opened to choose a file rather than to manage them.
#[derive(Clone, Debug)]
pub struct Picker {
    /// What the chosen file is for.
    pub target: PickTarget,
    /// Mime types offered; directories are always shown.
    pub mimes: Vec<&'static str>,
}

/// The files cut or copied, awaiting a paste.
#[derive(Clone, Debug)]
pub struct Clipboard {
    /// The files.
    pub paths: Vec<PathBuf>,
    /// Whether the paste moves rather than copies.
    pub cut: bool,
}

/// A press that may become a click or a drag.
#[derive(Clone, Copy, Debug)]
pub struct Press {
    /// Where the button went down.
    pub x: f64,
    /// Where the button went down.
    pub y: f64,
    /// The item under it, if any.
    pub item: Option<usize>,
    /// Whether the item was already selected, so a release without a drag
    /// narrows the selection to it.
    pub was_selected: bool,
}

/// An area of a window that scrolls on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollArea {
    /// The places in the sidebar.
    Sidebar,
    /// The folder's items.
    Content,
}

/// An item being renamed.
#[derive(Clone, Debug)]
pub struct Rename {
    /// The item.
    pub index: usize,
    /// The text typed so far.
    pub text: String,
    /// Whether the insertion caret is currently visible.
    pub caret_visible: bool,
    /// When the caret should next change visibility.
    pub blink_at: Instant,
}

/// An action offered by the context menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// Open the item or the selection.
    Open,
    /// Open the selection with a desktop entry.
    OpenWith(String),
    /// Open a new window on the folder.
    OpenInNewWindow,
    /// Cut the selection.
    Cut,
    /// Copy the selection.
    Copy,
    /// Paste into the folder.
    Paste,
    /// Rename the item.
    Rename,
    /// Delete the selection after confirmation.
    Delete,
    /// Create a folder.
    NewFolder,
    /// Create an empty file.
    NewFile,
    /// Open the selected file in the default text editor.
    Edit,
    /// Show file properties.
    Properties(PathBuf),
    /// Run an executable file.
    RunFile(PathBuf),
    /// Run an executable file in a terminal.
    RunFileTerminal(PathBuf),
    /// Toggle hidden files.
    ToggleHidden,
    /// Switch the view.
    View(View),
    /// Sort by a key.
    Sort(SortKey),
    /// Open the folder in a terminal.
    Terminal,
}

/// One row of the context menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuRow {
    /// A clickable action.
    Action(String, MenuAction, bool),
    /// A separator.
    Separator,
}

/// The context menu open in a window.
#[derive(Clone, Debug)]
pub struct ContextMenu {
    /// Where it was opened, in window coordinates.
    pub x: i32,
    /// Where it was opened, in window coordinates.
    pub y: i32,
    /// Its rows.
    pub rows: Vec<MenuRow>,
}

/// A deletion waiting for confirmation.
#[derive(Clone, Debug)]
pub struct Confirm {
    /// The files.
    pub paths: Vec<PathBuf>,
}

/// File properties currently being inspected or edited.
#[derive(Clone, Debug)]
pub struct Properties {
    /// The file or directory.
    pub path: PathBuf,
    /// Unix permission bits (owner/group/other).
    pub mode: u32,
    /// File length or the sum of regular files below a directory.
    pub size: Option<u64>,
}

/// Where a drag over the window would drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// Into the folder shown.
    Folder,
    /// Into a folder item.
    Item(usize),
    /// Into a sidebar place.
    Place(usize),
}

/// An operation reported in the status bar.
#[derive(Clone, Debug)]
pub struct Progress {
    /// Files done.
    pub done: usize,
    /// Files in total.
    pub total: usize,
    /// The file being worked on.
    pub current: String,
}

/// One file manager window.
pub struct FilesWindow {
    /// The desktop's number for the window.
    pub id: u32,
    /// The folder shown.
    pub dir: PathBuf,
    /// Every item of the folder as read.
    all: Vec<Item>,
    /// The items shown, after the hidden filter and the typed filter, sorted.
    pub items: Vec<Item>,
    /// Selected rows of `items`.
    pub selection: HashSet<usize>,
    /// The row keyboard navigation and range selection start from.
    pub cursor: Option<usize>,
    /// Scroll offset of the content area.
    pub scroll: f64,
    /// Scroll offset of the sidebar.
    pub sidebar_scroll: f64,
    /// The view.
    pub view: View,
    /// The sort key.
    pub sort: SortKey,
    /// Whether the sort is ascending.
    pub ascending: bool,
    /// Whether hidden files are shown.
    pub show_hidden: bool,
    /// The typed filter.
    pub filter: String,
    /// Folders visited, for back and forward.
    pub history: Vec<PathBuf>,
    /// Position in the history.
    pub history_pos: usize,
    /// Size and scale.
    pub geometry: Geometry,
    /// The rendered pixels.
    pub pix: Option<Pixmap>,
    /// Whether the window needs drawing.
    pub dirty: bool,
    /// A press waiting to become a click or a drag.
    pub press: Option<Press>,
    /// The rename in progress.
    pub rename: Option<Rename>,
    /// The open context menu.
    pub menu: Option<ContextMenu>,
    /// The deletion awaiting confirmation.
    pub confirm: Option<Confirm>,
    /// The properties dialog.
    pub properties: Option<Properties>,
    /// The picker this window is, if it is one.
    pub picker: Option<Picker>,
    /// The path being typed into the location field.
    pub path_edit: Option<String>,
    /// A message in the status bar, shown until its time.
    pub status: Option<(String, Instant)>,
    /// Where a drag over the window would drop.
    pub drop_target: Option<DropTarget>,
    /// The last click, for double clicks.
    pub last_click: Option<(u32, usize)>,
    /// The scrollbar thumb being dragged, with the offset into it grabbed.
    pub bar_drag: Option<(ScrollArea, f64)>,
    /// The inotify watch on the folder.
    watch: i32,
    /// When the folder is read again after a change.
    rescan_at: Option<Instant>,
    /// The operation running for this window.
    pub progress: Option<(u64, Progress)>,
    /// The title last sent to the window manager.
    pub shown_title: String,
}

impl FilesWindow {
    /// The items of the folder as read, before filtering.
    pub fn all_items(&self) -> &[Item] {
        &self.all
    }

    /// The selected items in listing order.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let mut rows: Vec<usize> = self.selection.iter().copied().collect();
        rows.sort();
        rows.into_iter()
            .filter_map(|i| self.items.get(i))
            .map(|item| item.path.clone())
            .collect()
    }

    /// Whether the window may change files: a picker may not.
    pub fn editable(&self) -> bool {
        self.picker.is_none()
    }

    /// The title the window manager shows.
    pub fn title(&self) -> String {
        if let Some(picker) = &self.picker {
            return match picker.target {
                PickTarget::Wallpaper { .. } => tr("Choose a wallpaper").to_string(),
                PickTarget::StartIcon => tr("Choose a start menu icon").to_string(),
            };
        }
        display_name(&self.dir)
    }

    /// Applies the hidden flag, the typed filter and the sort to the listing,
    /// keeping the selection on the same paths where they remain.
    pub fn refilter(&mut self) {
        let selected: HashSet<PathBuf> = self.selected_paths().into_iter().collect();
        let cursor_path = self
            .cursor
            .and_then(|c| self.items.get(c))
            .map(|i| i.path.clone());
        let needle = self.filter.to_lowercase();
        let picker_mimes: Option<Vec<&'static str>> = self.picker.as_ref().map(|p| p.mimes.clone());
        let mut items: Vec<Item> = self
            .all
            .iter()
            .filter(|item| self.show_hidden || !item.hidden)
            .filter(|item| needle.is_empty() || item.name.to_lowercase().contains(&needle))
            .filter(|item| {
                picker_mimes
                    .as_ref()
                    .is_none_or(|mimes| item.is_dir || mimes.contains(&item.mime.as_str()))
            })
            .cloned()
            .collect();
        sort_items(&mut items, self.sort, self.ascending);
        self.items = items;
        self.selection = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| selected.contains(&item.path))
            .map(|(i, _)| i)
            .collect();
        self.cursor = cursor_path.and_then(|p| self.items.iter().position(|i| i.path == p));
        if let Some(rename) = &self.rename
            && rename.index >= self.items.len()
        {
            self.rename = None;
        }
        self.dirty = true;
    }
}

/// Everything the file manager keeps across its windows.
pub struct Manager {
    /// The open windows.
    pub windows: Vec<FilesWindow>,
    next_id: u32,
    /// The mime database.
    pub mime: MimeDb,
    /// The cut or copied files.
    pub clipboard: Option<Clipboard>,
    /// The background workers.
    pub worker: Worker,
    /// Decoded thumbnails; `None` for images that could not be decoded.
    thumbs: HashMap<ThumbKey, Option<Rc<Pixmap>>>,
    /// The inotify instance watching the shown folders.
    inotify: RawFd,
    /// The window a drag started from and the files it carries, while it lasts.
    pub drag: Option<(u32, Vec<PathBuf>)>,
    /// The sidebar places, read once.
    pub places: Vec<Place>,
    /// Operations in flight, by number, with the window they report to.
    operations: HashMap<u64, u32>,
}

impl Default for Manager {
    fn default() -> Manager {
        Manager::new()
    }
}

impl Manager {
    /// Loads the mime database and starts the workers.
    pub fn new() -> Manager {
        let inotify = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        Manager {
            windows: Vec::new(),
            next_id: 1,
            mime: MimeDb::load(),
            clipboard: None,
            worker: Worker::new(),
            thumbs: HashMap::new(),
            inotify,
            drag: None,
            places: places(),
            operations: HashMap::new(),
        }
    }

    /// The descriptors the main loop polls: the worker pipe and the watch.
    pub fn fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.worker.fd()];
        if self.inotify >= 0 {
            fds.push(self.inotify);
        }
        fds
    }

    /// The window with a number.
    pub fn window(&self, id: u32) -> Option<&FilesWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window with a number, mutably.
    pub fn window_mut(&mut self, id: u32) -> Option<&mut FilesWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// The numbers of every window.
    pub fn ids(&self) -> Vec<u32> {
        self.windows.iter().map(|w| w.id).collect()
    }

    /// Creates a window on a folder, not yet given a surface.
    ///
    /// # Arguments
    /// * `dir` - The folder, or `None` for the home directory.
    /// * `picker` - What the window picks for, when it is one.
    /// * `defaults` - The view, sort and hidden setting to start with.
    ///
    /// # Returns
    /// The window's number.
    pub fn open(&mut self, dir: Option<PathBuf>, picker: Option<Picker>, defaults: Defaults) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let dir = dir.filter(|d| d.is_dir()).unwrap_or_else(config::home);
        let mut window = FilesWindow {
            id,
            dir: PathBuf::new(),
            all: Vec::new(),
            items: Vec::new(),
            selection: HashSet::new(),
            cursor: None,
            scroll: 0.0,
            sidebar_scroll: 0.0,
            view: defaults.view,
            sort: defaults.sort,
            ascending: true,
            show_hidden: defaults.show_hidden,
            filter: String::new(),
            history: Vec::new(),
            history_pos: 0,
            geometry: Geometry::default(),
            pix: None,
            dirty: true,
            press: None,
            rename: None,
            menu: None,
            confirm: None,
            properties: None,
            picker,
            path_edit: None,
            status: None,
            drop_target: None,
            last_click: None,
            bar_drag: None,
            watch: -1,
            rescan_at: None,
            progress: None,
            shown_title: String::new(),
        };
        self.load_dir(&mut window, &dir);
        window.history.push(dir);
        self.windows.push(window);
        id
    }

    /// Closes a window and drops its watch.
    pub fn close(&mut self, id: u32) {
        if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            let window = self.windows.remove(index);
            self.unwatch(window.watch);
        }
    }

    /// Reads the contents of a folder into a window.
    fn load_dir(&mut self, window: &mut FilesWindow, dir: &Path) {
        if window.watch >= 0 {
            self.unwatch(window.watch);
        }
        window.dir = dir.to_path_buf();
        window.watch = self.watch(dir);
        match read_dir(dir) {
            Ok(items) => {
                window.all = items;
                window.scroll = 0.0;
                window.rescan_at = None;
                window.refilter();
            }
            Err(error) => {
                window.status = Some((format!("{}: {error}", display_name(&dir)), Instant::now()));
                window.dirty = true;
            }
        }
    }

    /// Shows a folder in a window and records it in the history.
    ///
    /// # Arguments
    /// * `id` - The window.
    /// * `dir` - The folder.
    pub fn navigate(&mut self, id: u32, dir: &Path) {
        let Some(index) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        let mut window = self.windows.remove(index);
        if dir.is_dir() {
            self.load_dir(&mut window, dir);
            window.history.truncate(window.history_pos + 1);
            if window.history.last() != Some(&window.dir) {
                window.history.push(window.dir.clone());
            }
            window.history_pos = window.history.len() - 1;
            window.selection.clear();
            window.cursor = None;
        }
        self.windows.insert(index, window);
    }

    /// Steps back or forward in the history.
    ///
    /// # Arguments
    /// * `id` - The window.
    /// * `direction` - -1 for back, 1 for forward.
    pub fn step_history(&mut self, id: u32, direction: i32) {
        let Some(index) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        let mut window = self.windows.remove(index);
        let target = window.history_pos as i32 + direction;
        if target >= 0 && (target as usize) < window.history.len() {
            window.history_pos = target as usize;
            let dir = window.history[window.history_pos].clone();
            self.load_dir(&mut window, &dir);
            window.selection.clear();
            window.cursor = None;
        }
        self.windows.insert(index, window);
    }

    /// Refreshes a window's folder.
    pub fn refresh(&mut self, id: u32) {
        let Some(index) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        let mut window = self.windows.remove(index);
        let selected = window.selected_paths();
        self.load_dir(&mut window, &window.dir.clone());
        window.selection = window
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| selected.contains(&item.path))
            .map(|(i, _)| i)
            .collect();
        self.windows.insert(index, window);
    }

    /// Adds an inotify watch to a folder.
    fn watch(&self, path: &Path) -> i32 {
        if self.inotify < 0 {
            return -1;
        }
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return -1;
        };
        unsafe {
            libc::inotify_add_watch(
                self.inotify,
                path.as_ptr(),
                libc::IN_CREATE
                    | libc::IN_DELETE
                    | libc::IN_MOVED_FROM
                    | libc::IN_MOVED_TO
                    | libc::IN_CLOSE_WRITE
                    | libc::IN_ATTRIB
                    | libc::IN_DELETE_SELF
                    | libc::IN_MOVE_SELF,
            )
        }
    }

    /// Removes a watch.
    fn unwatch(&self, watch: i32) {
        if watch >= 0 {
            unsafe { libc::inotify_rm_watch(self.inotify, watch) };
        }
    }

    /// Reads inotify events and schedules rescans.
    pub fn read_events(&mut self) -> bool {
        if self.inotify < 0 {
            return false;
        }
        let mut buf = [0u8; 8192];
        let mut changed = HashSet::new();
        loop {
            let n = unsafe {
                libc::read(
                    self.inotify,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                let event = unsafe { &*(buf.as_ptr().add(offset) as *const libc::inotify_event) };
                changed.insert(event.wd);
                offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
            }
        }
        let due = Instant::now() + RESCAN_SETTLE;
        for window in &mut self.windows {
            if changed.contains(&window.watch) {
                window.rescan_at = Some(due);
            }
        }
        !changed.is_empty()
    }

    /// Takes the messages the workers sent and applies them: progress and
    /// results to their operations, thumbnails to the cache.
    pub fn drain_worker(&mut self) -> bool {
        let messages = self.worker.drain();
        let changed = !messages.is_empty();
        for message in messages {
            match message {
                Message::Progress { id, done, total, current } => {
                    if let Some(window_id) = self.operations.get(&id).copied()
                        && let Some(window) = self.window_mut(window_id)
                    {
                        window.progress = Some((id, Progress { done, total, current }));
                        window.dirty = true;
                    }
                }
                Message::Done { id, errors } => {
                    if let Some(window_id) = self.operations.remove(&id)
                        && let Some(window) = self.window_mut(window_id)
                    {
                        window.progress = None;
                        if !errors.is_empty() {
                            let first = errors.first().cloned().unwrap_or_default();
                            let more = if errors.len() > 1 {
                                format!(" (+{} more)", errors.len() - 1)
                            } else {
                                String::new()
                            };
                            window.status = Some((format!("{first}{more}"), Instant::now()));
                        }
                        window.dirty = true;
                    }
                    for window in &mut self.windows {
                        window.rescan_at = Some(Instant::now());
                    }
                }
                Message::Thumb { key, image } => {
                    self.thumbs.insert(key, image.map(Rc::new));
                    for window in &mut self.windows {
                        window.dirty = true;
                    }
                }
            }
        }
        changed
    }

    /// Runs the rescans that are due and expires old status messages.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn tick(&mut self, now: Instant) {
        let due: Vec<u32> = self
            .windows
            .iter()
            .filter(|w| w.rescan_at.is_some_and(|at| at <= now))
            .map(|w| w.id)
            .collect();
        for id in due {
            self.refresh(id);
        }
        for window in &mut self.windows {
            if window
                .status
                .as_ref()
                .is_some_and(|(_, since)| now.duration_since(*since) >= STATUS_LINGER)
            {
                window.status = None;
                window.dirty = true;
            }
            if let Some(rename) = window.rename.as_mut()
                && now >= rename.blink_at
            {
                rename.caret_visible = !rename.caret_visible;
                rename.blink_at = now + Duration::from_millis(500);
                window.dirty = true;
            }
        }
    }

    /// When `tick` next has something to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut deadline: Option<Instant> = None;
        for window in &self.windows {
            for candidate in [
                window.rescan_at,
                window.status.as_ref().map(|(_, since)| *since + STATUS_LINGER),
                window.rename.as_ref().map(|rename| rename.blink_at),
            ]
            .into_iter()
            .flatten()
            {
                deadline = Some(deadline.map_or(candidate, |d| d.min(candidate)));
            }
        }
        deadline
    }
}
