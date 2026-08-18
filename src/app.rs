//! Application state and the actions the keyboard drives.
//!
//! Everything the UI draws is derived from this struct; ui.rs never decides
//! anything. Long jobs (probing dropped files, merging) run on worker threads
//! and report back through `AppEvent`, so the frame loop never blocks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::collect::{self, AddEvent};
use crate::convert;
use crate::encoder::{EncoderPref, Quality};
use crate::fetch::{self, FetchEvent, FetchQuality, Stage};
use crate::ffmpeg::Tools;
use crate::format;
use crate::merge::{self, MergeEvent, Outcome, Step};
use crate::plan::{self, TargetOverride};
use crate::probe::ClipInfo;

#[derive(Debug)]
pub enum AppEvent {
    Add(AddEvent),
    Merge(MergeEvent),
    Fetch(FetchEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Good,
    Warn,
    Bad,
}

/// A clip plus whether the user has marked it for a bulk action.
pub struct Entry {
    pub clip: ClipInfo,
    pub marked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// Paths typed, pasted or dropped in.
    AddPaths,
    OutputName,
    CustomTarget,
    /// A link to download from.
    FetchUrl,
}

pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub hint: String,
    pub buffer: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TargetChoice {
    Auto,
    Fixed(TargetOverride),
    Custom,
}

pub enum MenuKind {
    Quality,
    Target(Vec<TargetChoice>),
    /// Which stream to take from a link. Unlike the first two this is not a
    /// setting being changed - picking one starts the download.
    Fetch,
    /// Which format to convert into. Also starts the job rather than storing a
    /// setting, for the same reason.
    Convert,
}

pub struct Menu {
    pub kind: MenuKind,
    pub title: String,
    pub note: String,
    pub items: Vec<(String, String)>,
    pub cursor: usize,
}

pub enum Confirm {
    Overwrite(PathBuf),
    CancelMerge,
    /// Stopping a batch conversion. Its own question because the answer is
    /// different: the files already converted are kept.
    CancelConvert,
    CancelFetch,
    /// Finishing a live capture on purpose. Not a cancellation in anything but
    /// the mechanism: it is how a recording ends, and it keeps the file.
    StopRecording,
}

/// One section of the key reference.
pub struct HelpGroup {
    pub title: &'static str,
    pub keys: Vec<(&'static str, String)>,
}

/// Everything the keyboard does. The hint bar carries the handful that get used
/// constantly; this carries the rest, on demand, so the bar can stay calm.
pub struct HelpSheet {
    pub groups: Vec<HelpGroup>,
}

impl HelpSheet {
    fn new() -> Self {
        let group = |title: &'static str, keys: Vec<(&'static str, &str)>| HelpGroup {
            title,
            keys: keys.into_iter().map(|(k, v)| (k, v.to_string())).collect(),
        };
        Self {
            groups: vec![
                group(
                    "The list",
                    vec![
                        ("↑ ↓  j k", "move the cursor"),
                        ("⇧↑ ⇧↓  J K", "move the selected clip"),
                        ("home end  g G", "first, last"),
                        ("space", "mark a clip"),
                        ("del   d", "remove marked, or selected"),
                        ("esc", "clear the marks"),
                    ],
                ),
                group(
                    "Clips",
                    vec![
                        ("a   f", "add files or a folder"),
                        ("u", "download from a link"),
                        ("v", "convert to another format"),
                        ("c", "clear the list"),
                        ("n", "sort by filename"),
                    ],
                ),
                group(
                    "The merge",
                    vec![
                        ("s", "start"),
                        ("o", "output name"),
                        ("q", "quality"),
                        ("t", "target size and framerate"),
                        ("e", "GPU if possible, else CPU"),
                        ("r", "force every clip re-encoded"),
                    ],
                ),
                group(
                    "Other",
                    vec![
                        ("m", "release the mouse for copying"),
                        ("x  ctrl+c", "exit"),
                    ],
                ),
            ],
        }
    }
}

pub enum Overlay {
    None,
    Prompt(Prompt),
    Menu(Menu),
    Confirm(Confirm),
    Help(HelpSheet),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegState {
    Queued,
    Running,
    Done,
    Failed,
}

pub struct SegRow {
    pub name: String,
    pub duration: f64,
    pub step: Step,
    pub done: f64,
    pub state: SegState,
    pub elapsed: f64,
}

/// What the progress screen shows. Rebuilt each time a merge or a conversion
/// starts.
///
/// Both jobs are the same shape on screen - a row per clip, a bar per row, one
/// overall bar - so they share the view rather than having two of it. What
/// differs is carried in `joins` and `label`: a conversion never joins anything,
/// so the last slice of the bar is not held back for a step that will not happen.
pub struct MergeView {
    /// What the header says after the counts: the output name for a merge, or the
    /// format everything is being converted to. A conversion has no single output
    /// to name - each file lands beside its own source, and those need not even be
    /// in one folder - so the format is the honest answer.
    pub label: String,
    /// Whether a join follows the per-clip pass. False for a conversion.
    pub joins: bool,
    pub plan: Vec<String>,
    pub rows: Vec<SegRow>,
    pub active: Option<usize>,
    pub joining: bool,
    pub join_done: f64,
    pub join_total: f64,
    pub attempt: u32,
    pub started: Instant,
    pub total_duration: f64,
}

impl MergeView {
    /// The prepare pass is the expensive part; the join is disk-bound and
    /// quick, so it gets the last slice of the bar rather than half of it.
    const JOIN_SHARE: f64 = 0.15;

    pub(crate) fn new(output: &Path, clips: &[Entry]) -> Self {
        let label = output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let clips: Vec<ClipInfo> = clips.iter().map(|e| e.clip.clone()).collect();
        Self::for_job(label, true, &clips)
    }

    /// The same screen for a conversion: one row per file, and no join at the end.
    pub(crate) fn converting(target: convert::Target, clips: &[ClipInfo]) -> Self {
        Self::for_job(format!("as {}", target.ext().to_uppercase()), false, clips)
    }

    fn for_job(label: String, joins: bool, clips: &[ClipInfo]) -> Self {
        Self {
            label,
            joins,
            plan: Vec::new(),
            rows: clips
                .iter()
                .map(|clip| SegRow {
                    name: clip.name.clone(),
                    duration: clip.duration,
                    step: Step::Copy,
                    done: 0.0,
                    state: SegState::Queued,
                    elapsed: 0.0,
                })
                .collect(),
            active: None,
            joining: false,
            join_done: 0.0,
            join_total: 0.0,
            attempt: 1,
            started: Instant::now(),
            total_duration: clips.iter().map(|c| c.duration).sum(),
        }
    }

    /// 0.0 to 1.0 across both phases.
    pub fn overall(&self) -> f64 {
        if self.total_duration <= 0.0 {
            return if self.joining { 0.9 } else { 0.0 };
        }
        // A conversion has no join to hold room for, so the per-file pass is the
        // whole bar. Reserving the last 15% for a step that never runs would
        // leave a finished job sitting at 85%.
        let join_share = if self.joins { Self::JOIN_SHARE } else { 0.0 };
        let prepared: f64 = self
            .rows
            .iter()
            .map(|r| match r.state {
                SegState::Done => r.duration,
                SegState::Running => r.done.min(r.duration),
                _ => 0.0,
            })
            .sum();
        let prepare = (prepared / self.total_duration).clamp(0.0, 1.0) * (1.0 - join_share);
        let join = if self.join_total > 0.0 {
            (self.join_done / self.join_total).clamp(0.0, 1.0) * join_share
        } else if self.joining {
            join_share * 0.5
        } else {
            0.0
        };
        (prepare + join).clamp(0.0, 1.0)
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Seconds left, once there is enough progress for the estimate to mean
    /// anything. Early guesses are wild, so below 2% it stays hidden.
    pub fn remaining(&self) -> Option<f64> {
        let fraction = self.overall();
        if fraction < 0.02 {
            return None;
        }
        let elapsed = self.elapsed();
        if elapsed < 1.0 {
            return None;
        }
        Some((elapsed / fraction - elapsed).max(0.0))
    }
}

/// What the download screen shows. Rebuilt each time a download starts.
pub struct FetchView {
    pub url: String,
    /// The video's own name, once the site has said what it is.
    pub title: Option<String>,
    pub stage: Stage,
    /// Which stream is arriving. Best video and best audio are usually separate
    /// files, so the byte count runs from zero more than once.
    pub stream: u32,
    pub done: u64,
    pub total: Option<u64>,
    pub rate: f64,
    pub eta: Option<f64>,
    /// Pieces done and pieces there are, for a stream that arrives in pieces.
    /// What a live broadcast is measured by: it has no byte total, but the site
    /// says how much of it has been broadcast so far.
    pub fragments: Option<(u64, u64)>,
    pub notes: Vec<String>,
    pub started: Instant,
}

impl FetchView {
    pub(crate) fn new(url: String) -> Self {
        Self {
            url,
            title: None,
            stage: Stage::Setup,
            stream: 0,
            done: 0,
            total: None,
            rate: 0.0,
            eta: None,
            fragments: None,
            notes: Vec::new(),
            started: Instant::now(),
        }
    }

    /// Whether what is on screen is a broadcast being captured as it happens.
    ///
    /// Read off the stage rather than remembered separately, so the screen and
    /// the worker cannot disagree about which of the two jobs is running.
    pub fn is_recording(&self) -> bool {
        matches!(self.stage, Stage::Recording)
    }

    /// 0.0 to 1.0 through the stream currently arriving, or None when the
    /// server has not said how big it is - a bar drawn without a total is a
    /// guess dressed up as a measurement.
    pub fn fraction(&self) -> Option<f64> {
        if let Some(total) = self.total.filter(|t| *t > 0) {
            return Some((self.done as f64 / total as f64).clamp(0.0, 1.0));
        }
        // No declared size, which is every live broadcast: nobody knows how big
        // something still happening will be. The pieces it arrives in are
        // counted though, and how many there are is a real number rather than an
        // estimate, so the bar drawn from them is measuring something.
        let (done, count) = self.fragments.filter(|(_, count)| *count > 0)?;
        Some((done as f64 / count as f64).clamp(0.0, 1.0))
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// What to call what is on screen, at whatever detail is known.
    pub fn what(&self) -> String {
        self.title.clone().unwrap_or_else(|| self.url.clone())
    }
}

pub enum Screen {
    Browse,
    Merging(MergeView),
    /// Files being converted one by one. Drawn by the merge screen, because it is
    /// the same picture.
    Converting(MergeView),
    Fetching(FetchView),
    Result(Box<Outcome>),
    /// The same report as `Result`, from a download rather than a merge. Kept
    /// apart only so that returning to the list does not renumber the merge
    /// output after a download that never touched it.
    Fetched(Box<Outcome>),
    /// And the same again for a conversion, which also leaves merged.mp4 alone.
    Converted(Box<Outcome>),
}

pub struct App {
    pub tools: Arc<Tools>,
    pub root: PathBuf,
    pub clips: Vec<Entry>,
    pub cursor: usize,
    pub output_name: String,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub target_override: Option<TargetOverride>,
    pub force_reencode: bool,
    pub screen: Screen,
    pub overlay: Overlay,
    pub status: Option<(String, Kind)>,
    /// Some(n) while a background probe is running: n files still to read.
    pub probing: Option<(usize, usize)>,
    pub cancel: Arc<AtomicBool>,
    /// Whether mouse capture is on. Off hands text selection back to the terminal.
    pub mouse: bool,
    pub quit: bool,
    /// Which stream the download picker opens on. Whatever was taken last time
    /// is usually what is wanted again.
    pub fetch_quality: FetchQuality,
    /// Which format the convert picker opens on, for the same reason: converting
    /// a folder usually means converting all of it the same way.
    pub convert_target: convert::Target,
    /// Where yt-dlp is installed if it has to be fetched, and where an existing
    /// copy is looked for. Set by main, because only main knows where the
    /// executable actually lives.
    pub tool_root: PathBuf,
    pub tool_search: Vec<PathBuf>,
    pub allow_ytdlp_download: bool,
    /// The link waiting for a quality to be picked.
    pending_url: Option<String>,
    tx: Sender<AppEvent>,
}

impl App {
    pub fn new(tools: Arc<Tools>, root: PathBuf, tx: Sender<AppEvent>) -> Self {
        Self {
            tools,
            clips: Vec::new(),
            cursor: 0,
            output_name: "merged.mp4".into(),
            quality: Quality::High,
            encoder: EncoderPref::Auto,
            target_override: None,
            force_reencode: false,
            screen: Screen::Browse,
            overlay: Overlay::None,
            status: None,
            probing: None,
            cancel: Arc::new(AtomicBool::new(false)),
            mouse: true,
            quit: false,
            fetch_quality: FetchQuality::P1080,
            convert_target: convert::Target::Mp4,
            tool_root: root.clone(),
            tool_search: vec![root.clone()],
            allow_ytdlp_download: true,
            pending_url: None,
            root,
            tx,
        }
    }

    pub fn say(&mut self, text: impl Into<String>, kind: Kind) {
        self.status = Some((text.into(), kind));
    }

    // ---------------------------------------------------------------- clips

    pub fn add_paths(&mut self, candidates: Vec<String>) {
        if candidates.is_empty() {
            return;
        }
        if self.probing.is_some() {
            self.say("Still reading the last batch - one moment.", Kind::Warn);
            return;
        }
        self.probing = Some((0, 0));
        collect::spawn_probe(self.tools.clone(), candidates, self.tx.clone(), AppEvent::Add);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.clips.is_empty() {
            return;
        }
        let last = self.clips.len() - 1;
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, last as isize) as usize;
    }

    pub fn cursor_to(&mut self, index: usize) {
        self.cursor = index.min(self.clips.len().saturating_sub(1));
    }

    /// Moves the clip under the cursor, and follows it. This is the action the
    /// whole list exists for, so it gets the arrow keys rather than a prompt.
    pub fn move_clip(&mut self, delta: isize) {
        if self.clips.len() < 2 {
            return;
        }
        let from = self.cursor;
        let to = (from as isize + delta).clamp(0, self.clips.len() as isize - 1) as usize;
        if from == to {
            return;
        }
        let entry = self.clips.remove(from);
        self.clips.insert(to, entry);
        self.cursor = to;
        self.say(format!("Moved to position {}.", to + 1), Kind::Good);
    }

    pub fn toggle_mark(&mut self) {
        if let Some(entry) = self.clips.get_mut(self.cursor) {
            entry.marked = !entry.marked;
        }
        self.move_cursor(1);
    }

    pub fn marked_count(&self) -> usize {
        self.clips.iter().filter(|e| e.marked).count()
    }

    /// Removes every marked clip, or the one under the cursor when nothing is
    /// marked - which is what a bare Del is expected to do.
    pub fn remove_selection(&mut self) {
        if self.clips.is_empty() {
            return;
        }
        let marked = self.marked_count();
        if marked == 0 {
            let removed = self.clips.remove(self.cursor);
            self.cursor_to(self.cursor);
            self.say(format!("Removed {}.", removed.name_short()), Kind::Good);
        } else {
            self.clips.retain(|e| !e.marked);
            self.cursor_to(self.cursor);
            self.say(format!("Removed {marked} clips."), Kind::Good);
        }
    }

    pub fn clear(&mut self) {
        self.clips.clear();
        self.cursor = 0;
        self.say("List cleared.", Kind::Good);
    }

    pub fn sort_by_name(&mut self) {
        if self.clips.len() < 2 {
            return;
        }
        let keep = self.clips.get(self.cursor).map(|e| e.clip.path.clone());
        self.clips.sort_by_key(|e| format::natural_key(&e.clip.name));
        if let Some(path) = keep
            && let Some(index) = self.clips.iter().position(|e| e.clip.path == path)
        {
            self.cursor = index;
        }
        self.say("Sorted by filename (1, 2, 10 - not 1, 10, 2).", Kind::Good);
    }

    pub fn toggle_encoder(&mut self) {
        self.encoder = self.encoder.toggled();
        self.say(
            format!("Encoder: {} ({})", self.encoder.label(), self.encoder.note()),
            Kind::Good,
        );
    }

    pub fn total_duration(&self) -> f64 {
        self.clips.iter().map(|e| e.clip.duration).sum()
    }

    pub fn total_size(&self) -> u64 {
        self.clips.iter().map(|e| e.clip.size_bytes).sum()
    }

    fn clip_list(&self) -> Vec<ClipInfo> {
        self.clips.iter().map(|e| e.clip.clone()).collect()
    }

    /// The one-line summary of what pressing S will actually do.
    pub fn plan_lines(&self) -> Vec<String> {
        if self.clips.is_empty() {
            return Vec::new();
        }
        let clips = self.clip_list();
        if self.clips.len() == 1 {
            return vec!["One clip - it just gets copied into an mp4.".into()];
        }
        if !self.force_reencode && plan::can_stream_copy(&clips) {
            return vec!["Fast join, nothing re-encoded - takes seconds.".into()];
        }
        let target = plan::target_format(&clips, self.target_override);
        let convert = plan::convert_count(&clips, &target);
        let source = if self.target_override.is_some() { "your choice" } else { "auto" };
        vec![
            format!("Convert to {} ({source}), then join.", target.label()),
            format!(
                "{convert} of {} clips need converting - press T to change the target.",
                self.clips.len()
            ),
        ]
    }

    // -------------------------------------------------------------- overlays

    pub fn prompt_add(&mut self) {
        self.overlay = Overlay::Prompt(Prompt {
            kind: PromptKind::AddPaths,
            title: "Add clips".into(),
            // What to do, not which keys: the buttons underneath say that.
            hint: "Drag files or a folder in, or paste a path.".into(),
            buffer: String::new(),
        });
    }

    /// Starts an add prompt already holding text - how a drop onto the list
    /// screen is turned into an editable line the user can check before adding.
    pub fn prompt_add_with(&mut self, text: String) {
        self.prompt_add();
        if let Overlay::Prompt(p) = &mut self.overlay {
            p.buffer = text;
        }
    }

    pub fn prompt_output(&mut self) {
        self.overlay = Overlay::Prompt(Prompt {
            kind: PromptKind::OutputName,
            title: "Output file name".into(),
            hint: "A name, or a full path. .mp4 is added if you leave the extension off.".into(),
            buffer: self.output_name.clone(),
        });
    }

    /// Asks for a link. Deliberately its own prompt rather than a URL smuggled
    /// through the add prompt: what happens next is a download and then nothing,
    /// which is not what "add clips" promises.
    pub fn prompt_fetch(&mut self) {
        if matches!(self.screen, Screen::Merging(_) | Screen::Converting(_) | Screen::Fetching(_)) {
            return;
        }
        self.overlay = Overlay::Prompt(Prompt {
            kind: PromptKind::FetchUrl,
            title: "Download a video".into(),
            hint: "Paste a link. YouTube and most other video sites work.".into(),
            buffer: String::new(),
        });
    }

    /// The stream picker. Opening it is the second half of `prompt_fetch`, so
    /// this is where the download actually starts from.
    fn menu_fetch(&mut self) {
        let items = FetchQuality::ALL
            .iter()
            .map(|q| (q.label().to_string(), q.note().to_string()))
            .collect();
        let cursor =
            FetchQuality::ALL.iter().position(|q| *q == self.fetch_quality).unwrap_or(1);
        self.overlay = Overlay::Menu(Menu {
            kind: MenuKind::Fetch,
            title: "How much of it".into(),
            note: "Bigger takes longer to fetch. Above 1080p there is no H.264, so those \
                   arrive as VP9 and joining one re-encodes it."
                .into(),
            items,
            cursor,
        });
    }

    /// The format picker. Like the download picker, choosing an item starts the
    /// job rather than remembering a preference for later.
    pub fn menu_convert(&mut self) {
        if matches!(self.screen, Screen::Merging(_) | Screen::Converting(_) | Screen::Fetching(_)) {
            return;
        }
        if self.clips.is_empty() {
            self.say("Nothing to convert yet - press A or drag some files in.", Kind::Warn);
            return;
        }
        if self.probing.is_some() {
            self.say("Still reading files - one moment.", Kind::Warn);
            return;
        }

        let chosen = self.convert_selection();
        let items = convert::Target::ALL
            .iter()
            .map(|t| (t.label().to_string(), t.note().to_string()))
            .collect();
        let cursor = convert::Target::ALL
            .iter()
            .position(|t| *t == self.convert_target)
            .unwrap_or(0);
        let marked = self.marked_count();
        self.overlay = Overlay::Menu(Menu {
            kind: MenuKind::Convert,
            title: if marked > 0 {
                format!("Convert {marked} marked file(s) to")
            } else {
                format!("Convert {} file(s) to", chosen.len())
            },
            note: "Each one becomes a file of its own beside the original, at the same size and \
                   length. Nothing is merged, and nothing is replaced."
                .into(),
            items,
            cursor,
        });
    }

    /// Which files a conversion would act on: the marked ones if any are marked,
    /// and otherwise all of them. The same rule the delete key follows, for the
    /// same reason - marking is how a subset gets picked out.
    fn convert_selection(&self) -> Vec<ClipInfo> {
        if self.marked_count() > 0 {
            self.clips.iter().filter(|e| e.marked).map(|e| e.clip.clone()).collect()
        } else {
            self.clip_list()
        }
    }

    pub fn menu_quality(&mut self) {
        let items = Quality::ALL
            .iter()
            .map(|q| (q.label().to_string(), q.note().to_string()))
            .collect();
        let cursor = Quality::ALL.iter().position(|q| *q == self.quality).unwrap_or(1);
        self.overlay = Overlay::Menu(Menu {
            kind: MenuKind::Quality,
            title: "Quality".into(),
            note: "Only matters for clips that get re-encoded.".into(),
            items,
            cursor,
        });
    }

    pub fn menu_target(&mut self) {
        if self.clips.is_empty() {
            self.say("Add some clips first - the choices depend on what they are.", Kind::Warn);
            return;
        }
        let clips = self.clip_list();
        let auto = plan::target_format(&clips, None);
        let biggest = clips
            .iter()
            .max_by_key(|c| c.width as u64 * c.height as u64)
            .expect("clips is not empty");
        let smallest = clips
            .iter()
            .min_by_key(|c| c.width as u64 * c.height as u64)
            .expect("clips is not empty");

        let describe = |c: &ClipInfo| {
            format!(
                "{}{}{} @ {} fps",
                c.width,
                crate::theme::glyph::TIMES,
                c.height,
                format::fps(c.fps)
            )
        };

        let choices = vec![
            TargetChoice::Auto,
            TargetChoice::Fixed(TargetOverride {
                width: biggest.width,
                height: biggest.height,
                fps: biggest.fps,
            }),
            TargetChoice::Fixed(TargetOverride {
                width: smallest.width,
                height: smallest.height,
                fps: smallest.fps,
            }),
            TargetChoice::Fixed(TargetOverride { width: 1920, height: 1080, fps: 30.0 }),
            TargetChoice::Fixed(TargetOverride { width: 1280, height: 720, fps: 30.0 }),
            TargetChoice::Custom,
        ];
        let items = vec![
            (format!("Auto - matches most of your footage: {}", auto.label()), String::new()),
            (format!("Biggest clip: {}", describe(biggest)), String::new()),
            (format!("Smallest clip - fastest: {}", describe(smallest)), String::new()),
            (format!("1920{}1080 @ 30 fps", crate::theme::glyph::TIMES), String::new()),
            (format!("1280{}720 @ 30 fps", crate::theme::glyph::TIMES), String::new()),
            ("Type your own...".to_string(), "e.g. 1080x1920@25".into()),
        ];

        self.overlay = Overlay::Menu(Menu {
            kind: MenuKind::Target(choices),
            title: "Target size and framerate".into(),
            note: "Everything is converted to one shape. Smaller and slower means a faster merge."
                .into(),
            items,
            cursor: 0,
        });
    }

    pub fn close_overlay(&mut self) {
        // Backing out of the stream picker abandons the link with it. It was
        // only ever held for the picker to finish the job, and a link left
        // lying about would attach itself to the next thing picked.
        if matches!(&self.overlay, Overlay::Menu(menu) if matches!(menu.kind, MenuKind::Fetch)) {
            self.pending_url = None;
        }
        self.overlay = Overlay::None;
    }

    /// Opens the key reference, or closes it if it is already up.
    pub fn toggle_help(&mut self) {
        if matches!(self.overlay, Overlay::Help(_)) {
            self.close_overlay();
        } else {
            self.overlay = Overlay::Help(HelpSheet::new());
        }
    }

    pub fn submit_prompt(&mut self) {
        let Overlay::Prompt(prompt) = &self.overlay else {
            return;
        };
        let kind = prompt.kind;
        let text = prompt.buffer.trim().to_string();
        self.close_overlay();

        if text.is_empty() {
            return;
        }

        match kind {
            PromptKind::AddPaths => {
                let candidates = collect::split_path_line(&text);
                if candidates.is_empty() {
                    self.say("Nothing in that looked like a path.", Kind::Warn);
                } else {
                    self.add_paths(candidates);
                }
            }
            PromptKind::OutputName => self.set_output_name(&text),
            PromptKind::CustomTarget => self.set_custom_target(&text),
            PromptKind::FetchUrl => match fetch::normalise_url(&text) {
                Some(url) => {
                    self.pending_url = Some(url);
                    self.menu_fetch();
                }
                None => self.say(
                    "That does not look like a link. It wants something like \
                     https://www.youtube.com/watch?v=...",
                    Kind::Warn,
                ),
            },
        }
    }

    fn set_output_name(&mut self, raw: &str) {
        let text = raw.trim().trim_matches('"');
        let path = Path::new(text);

        // Reject a name Windows cannot store here, rather than at the end of a
        // long merge. A full path is allowed, so only its last part is checked.
        let Some(leaf) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            self.say("That does not end in a file name.", Kind::Warn);
            return;
        };
        const ILLEGAL: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
        if leaf.chars().any(|c| ILLEGAL.contains(&c) || (c as u32) < 32) {
            self.say("A file name cannot contain any of  \\ / : * ? \" < > |", Kind::Warn);
            return;
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.is_dir()
        {
            self.say(format!("There is no folder called {}", parent.display()), Kind::Warn);
            return;
        }

        let mut name = text.to_string();
        if Path::new(&name).extension().is_none() {
            name.push_str(".mp4");
        }
        self.output_name = name.clone();
        self.say(format!("Output name set to {name}"), Kind::Good);
    }

    fn set_custom_target(&mut self, text: &str) {
        match parse_target_spec(text) {
            Some((width, height, fps)) => {
                let fps = fps.unwrap_or_else(|| {
                    plan::target_format(&self.clip_list(), None).fps
                });
                self.target_override = Some(TargetOverride { width, height, fps });
                self.announce_target();
            }
            None => self.say(
                format!("Could not read '{text}'. Use something like 1920x1080@30."),
                Kind::Warn,
            ),
        }
    }

    fn announce_target(&mut self) {
        let target = plan::target_format(&self.clip_list(), self.target_override);
        let suffix = if self.target_override.is_none() { " (auto)" } else { "" };
        self.say(format!("Target set to {}{suffix}", target.label()), Kind::Good);
    }

    pub fn menu_move(&mut self, delta: isize) {
        if let Overlay::Menu(menu) = &mut self.overlay {
            let last = menu.items.len().saturating_sub(1) as isize;
            menu.cursor = (menu.cursor as isize + delta).clamp(0, last) as usize;
        }
    }

    pub fn menu_pick(&mut self, index: Option<usize>) {
        let Overlay::Menu(menu) = &self.overlay else {
            return;
        };
        let index = index.unwrap_or(menu.cursor);
        if index >= menu.items.len() {
            return;
        }

        match &menu.kind {
            MenuKind::Quality => {
                let quality = Quality::ALL[index];
                self.close_overlay();
                self.quality = quality;
                self.say(format!("Quality set to {}", quality.label()), Kind::Good);
            }
            MenuKind::Convert => {
                let target = convert::Target::ALL[index];
                self.close_overlay();
                self.convert_target = target;
                self.launch_convert(target);
            }
            MenuKind::Fetch => {
                let quality = FetchQuality::ALL[index];
                // Taken before the overlay closes: closing the picker is also
                // how the link gets abandoned.
                let url = self.pending_url.take();
                self.close_overlay();
                self.fetch_quality = quality;
                if let Some(url) = url {
                    self.launch_fetch(url, quality);
                }
            }
            MenuKind::Target(choices) => {
                let choice = choices[index];
                self.close_overlay();
                match choice {
                    TargetChoice::Auto => {
                        self.target_override = None;
                        self.announce_target();
                    }
                    TargetChoice::Fixed(over) => {
                        self.target_override = Some(over);
                        self.announce_target();
                    }
                    TargetChoice::Custom => {
                        self.overlay = Overlay::Prompt(Prompt {
                            kind: PromptKind::CustomTarget,
                            title: "Size and framerate".into(),
                            hint: "WxH@fps, e.g. 1080x1920@25. The @fps part is optional.".into(),
                            buffer: String::new(),
                        });
                    }
                }
            }
        }
    }

    // ----------------------------------------------------------------- merge

    /// Where the merge will write, given the current output name.
    pub fn resolved_output(&self) -> Option<PathBuf> {
        let first = self.clips.first()?;
        let folder = first.clip.path.parent().unwrap_or(&self.root).to_path_buf();
        let mut name = self.output_name.clone();
        if Path::new(&name).extension().is_none() {
            name.push_str(".mp4");
        }
        let candidate = Path::new(&name);
        Some(if candidate.is_absolute() { candidate.to_path_buf() } else { folder.join(name) })
    }

    pub fn request_merge(&mut self) {
        if self.clips.is_empty() {
            self.say("Nothing to merge yet - press A or drag some files in.", Kind::Warn);
            return;
        }
        if self.probing.is_some() {
            self.say("Still reading files - one moment.", Kind::Warn);
            return;
        }
        // Sound with no picture cannot be joined to anything. Said here, before a
        // merge screen appears and fails one clip at a time.
        if let Some(soundtrack) = self.clips.iter().find(|e| !e.clip.has_video) {
            self.say(
                format!(
                    "{} has no picture, so it cannot be merged. Remove it, or press V to convert it.",
                    soundtrack.clip.name
                ),
                Kind::Bad,
            );
            return;
        }
        let Some(output) = self.resolved_output() else {
            return;
        };
        // Caught here as well as in the engine, so the answer arrives before a
        // merge screen appears and fails.
        if let Some(clash) = self.clips.iter().find(|e| merge::same_file(&e.clip.path, &output)) {
            self.say(
                format!("That output name is {} - one of the clips. Press O for another name.", clash.clip.name),
                Kind::Bad,
            );
            return;
        }
        if output.exists() {
            self.overlay = Overlay::Confirm(Confirm::Overwrite(output));
            return;
        }
        self.launch_merge(output);
    }

    /// Declining an overwrite writes beside the existing file instead of
    /// silently doing nothing.
    pub fn merge_next_to(&mut self, existing: &Path) {
        let folder = existing.parent().unwrap_or(&self.root).to_path_buf();
        let output = collect::default_output_path(&folder);
        if let Some(name) = output.file_name() {
            self.output_name = name.to_string_lossy().into_owned();
        }
        self.launch_merge(output);
    }

    pub fn launch_merge(&mut self, output: PathBuf) {
        self.close_overlay();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.screen = Screen::Merging(MergeView::new(&output, &self.clips));

        let job = merge::Job {
            tools: self.tools.clone(),
            clips: self.clip_list(),
            output,
            quality: self.quality,
            encoder: self.encoder,
            force_reencode: self.force_reencode,
            target_override: self.target_override,
        };
        merge::spawn(job, self.cancel.clone(), self.tx.clone(), AppEvent::Merge);
    }

    // -------------------------------------------------------------- convert

    pub fn launch_convert(&mut self, target: convert::Target) {
        let clips = self.convert_selection();
        if clips.is_empty() {
            return;
        }
        self.close_overlay();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.screen = Screen::Converting(MergeView::converting(target, &clips));

        let job = convert::Job {
            tools: self.tools.clone(),
            clips,
            target,
            quality: self.quality,
            encoder: self.encoder,
            force_reencode: self.force_reencode,
        };
        convert::spawn(job, self.cancel.clone(), self.tx.clone(), AppEvent::Merge);
    }

    // -------------------------------------------------------------- downloads

    pub fn launch_fetch(&mut self, url: String, quality: FetchQuality) {
        // Caught here rather than after a screen full of progress: a read-only
        // folder cannot take the finished file however well the download goes.
        if !fetch::folder_is_usable(&self.root) {
            self.say(
                format!("Nothing can be written to {} - pick another folder.", self.root.display()),
                Kind::Bad,
            );
            return;
        }
        self.close_overlay();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.screen = Screen::Fetching(FetchView::new(url.clone()));

        let job = fetch::Job {
            tools: self.tools.clone(),
            url,
            folder: self.root.clone(),
            quality,
            install_root: self.tool_root.clone(),
            search: self.tool_search.clone(),
            allow_download: self.allow_ytdlp_download,
        };
        fetch::spawn(job, self.cancel.clone(), self.tx.clone(), AppEvent::Fetch);
    }

    pub fn request_cancel(&mut self) {
        match &self.screen {
            Screen::Merging(_) => self.overlay = Overlay::Confirm(Confirm::CancelMerge),
            Screen::Converting(_) => self.overlay = Overlay::Confirm(Confirm::CancelConvert),
            Screen::Fetching(view) => {
                // Stopping a recording keeps what it has and stopping a download
                // throws it away, so the two cannot share a question.
                self.overlay = Overlay::Confirm(if view.is_recording() {
                    Confirm::StopRecording
                } else {
                    Confirm::CancelFetch
                });
            }
            _ => {}
        }
    }

    pub fn confirm_cancel(&mut self) {
        self.close_overlay();
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The report on screen, whichever of the three jobs produced it.
    pub fn outcome(&self) -> Option<&Outcome> {
        match &self.screen {
            Screen::Result(outcome) | Screen::Fetched(outcome) | Screen::Converted(outcome) => {
                Some(outcome)
            }
            _ => None,
        }
    }

    /// Back to the list after a merge, with the output name advanced so a
    /// second merge does not immediately ask about overwriting the first.
    ///
    /// A finished *download* leaves the name alone: it wrote a file of its own
    /// choosing and never went near merged.mp4, so renumbering would throw away
    /// an output name the user had set for a merge they have not run yet.
    pub fn dismiss_result(&mut self) {
        if let Screen::Result(outcome) = &self.screen
            && outcome.ok
        {
            let folder = outcome.output.parent().unwrap_or(&self.root).to_path_buf();
            if let Some(name) = collect::default_output_path(&folder).file_name() {
                self.output_name = name.to_string_lossy().into_owned();
            }
        }
        self.screen = Screen::Browse;
    }

    pub fn open_output_folder(&self) {
        if let Some(outcome) = self.outcome().filter(|o| o.ok) {
            reveal(&outcome.output);
        }
    }

    // ---------------------------------------------------------------- events

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Add(add) => self.handle_add(add),
            AppEvent::Merge(merge) => self.handle_merge(merge),
            AppEvent::Fetch(fetch) => self.handle_fetch(fetch),
        }
    }

    fn handle_fetch(&mut self, event: FetchEvent) {
        // A finished download switches screens; everything else updates the view.
        if let FetchEvent::Finished(outcome) = event {
            let kind = if outcome.ok { Kind::Good } else { Kind::Bad };
            // What was produced comes before how it ended. Stopping a recording
            // is how a recording is supposed to finish, so a file that exists
            // gets announced as one whether or not the user pressed esc to get
            // it - anything else would report a successful capture as a loss.
            let message = if outcome.ok {
                let verb = if outcome.recorded { "Recorded" } else { "Downloaded" };
                match outcome.output.file_name() {
                    Some(name) => format!("{verb} {}", name.to_string_lossy()),
                    None => "Finished.".to_string(),
                }
            } else if outcome.cancelled {
                "Download stopped.".to_string()
            } else {
                "That download failed.".to_string()
            };
            self.say(message, kind);
            self.screen = Screen::Fetched(outcome);
            self.close_overlay();
            return;
        }

        let Screen::Fetching(view) = &mut self.screen else {
            return;
        };

        match event {
            FetchEvent::Note(line) => view.notes.push(line),
            FetchEvent::Title(title) => view.title = Some(title),
            FetchEvent::Stage(stage) => {
                // The remux that follows a recording counts its own bytes from
                // zero, and leaving the recording's total on screen would make
                // the bar jump backwards from full.
                if stage == Stage::Finishing && view.is_recording() {
                    view.done = 0;
                    view.total = None;
                    view.fragments = None;
                }
                view.stage = stage;
            }
            FetchEvent::Stream(n) => {
                view.stream = n;
                // A stream starting *is* the download starting - derived here
                // rather than relying on a Stage event arriving alongside, which
                // would leave the screen saying "setting up" if it ever did not.
                view.stage = Stage::Downloading;
                // The count restarts with the stream, or the second one opens at
                // whatever the first one finished on.
                view.done = 0;
                view.total = None;
                view.eta = None;
                view.fragments = None;
            }
            FetchEvent::Progress { done, total, rate, eta, fragments } => {
                view.done = done;
                view.total = total;
                view.rate = rate;
                view.eta = eta;
                // Kept rather than overwritten with nothing: yt-dlp reports the
                // counts only while a fragmented stream is actually moving, and
                // a bar that vanished between two lines would flicker.
                if fragments.is_some() {
                    view.fragments = fragments;
                }
            }
            FetchEvent::Finished(_) => unreachable!("handled above"),
        }
    }

    fn handle_add(&mut self, event: AddEvent) {
        match event {
            AddEvent::Started(total) => self.probing = Some((0, total)),
            AddEvent::Added(info) => {
                self.clips.push(Entry { clip: *info, marked: false });
                if let Some((done, total)) = &mut self.probing {
                    *done += 1;
                    self.status = Some((
                        format!("Reading clips... {done}/{total}"),
                        Kind::Info,
                    ));
                }
            }
            AddEvent::Rejected { name, why } => {
                self.say(format!("Skipped {name}: {why}"), Kind::Warn);
            }
            AddEvent::Finished { added, rejected } => {
                self.probing = None;
                let message = match (added, rejected) {
                    (0, 0) => "Nothing to add.".to_string(),
                    (0, r) => format!("Nothing added - {r} item(s) could not be used."),
                    (a, 0) => format!("Added {a} clip(s)."),
                    (a, r) => format!("Added {a} clip(s). {r} skipped."),
                };
                let kind = match (added, rejected) {
                    (0, _) => Kind::Bad,
                    (_, 0) => Kind::Good,
                    _ => Kind::Warn,
                };
                self.say(message, kind);
                self.cursor_to(self.cursor);
            }
        }
    }

    /// Both jobs report through `MergeEvent`, so which of them is running is read
    /// off the screen rather than tracked separately: the two cannot disagree.
    fn handle_merge(&mut self, event: MergeEvent) {
        let converting = matches!(self.screen, Screen::Converting(_));

        // A finished job switches screens; everything else updates the view.
        if let MergeEvent::Finished(outcome) = event {
            let kind = if outcome.ok { Kind::Good } else { Kind::Bad };
            // What was produced comes before how it ended, the same as a
            // recording: a batch stopped half way still converted real files.
            let message = if converting {
                if outcome.ok {
                    match outcome.outputs.len() {
                        1 => format!(
                            "Converted {}",
                            outcome
                                .output
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                        n => format!("Converted {n} files."),
                    }
                } else if outcome.cancelled {
                    "Conversion stopped.".to_string()
                } else {
                    "That conversion failed.".to_string()
                }
            } else if outcome.cancelled {
                "Merge cancelled.".to_string()
            } else if outcome.ok {
                "Merge finished.".to_string()
            } else {
                "That merge failed.".to_string()
            };
            self.say(message, kind);
            self.screen =
                if converting { Screen::Converted(outcome) } else { Screen::Result(outcome) };
            self.close_overlay();
            return;
        }

        let (Screen::Merging(view) | Screen::Converting(view)) = &mut self.screen else {
            return;
        };

        match event {
            MergeEvent::Plan(line) => view.plan.push(line),
            MergeEvent::Pass { total, attempt } => {
                view.attempt = attempt;
                if attempt > 1 {
                    // A fallback pass starts the rows over. Without this the
                    // retry inherits the finished state of the pass that just
                    // failed, so it opens at nearly 100%.
                    for row in view.rows.iter_mut() {
                        row.state = SegState::Queued;
                        row.done = 0.0;
                        row.elapsed = 0.0;
                    }
                    view.joining = false;
                    view.join_done = 0.0;
                    view.join_total = 0.0;
                }
                debug_assert_eq!(total, view.rows.len(), "a pass covers every clip");
            }
            MergeEvent::SegmentStart { index, name, step, duration } => {
                if let Some(row) = view.rows.get_mut(index) {
                    row.name = name;
                    row.step = step;
                    row.duration = duration;
                    row.done = 0.0;
                    row.state = SegState::Running;
                }
                view.active = Some(index);
            }
            MergeEvent::SegmentProgress { index, done } => {
                if let Some(row) = view.rows.get_mut(index) {
                    row.done = done;
                }
            }
            MergeEvent::SegmentEnd { index, step, ok, elapsed } => {
                if let Some(row) = view.rows.get_mut(index) {
                    row.step = step;
                    row.state = if ok { SegState::Done } else { SegState::Failed };
                    row.elapsed = elapsed;
                    row.done = row.duration;
                }
                view.active = None;
            }
            MergeEvent::JoinStart => {
                view.joining = true;
                view.active = None;
            }
            MergeEvent::JoinProgress { done, total } => {
                view.join_done = done;
                view.join_total = total;
            }
            MergeEvent::Warning(text) => view.plan.push(text),
            MergeEvent::Finished(_) => unreachable!("handled above"),
        }
    }
}

impl Entry {
    fn name_short(&self) -> String {
        format::ellipsize(&self.clip.name, 40)
    }
}

/// "1080x1920@25" -> (1080, 1920, Some(25.0)). The @fps part is optional.
pub fn parse_target_spec(text: &str) -> Option<(u32, u32, Option<f64>)> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let (size, fps) = match cleaned.split_once('@') {
        Some((s, f)) => {
            let parsed: f64 = f.parse().ok()?;
            if !(1.0..=240.0).contains(&parsed) {
                return None;
            }
            (s.to_string(), Some(parsed))
        }
        None => (cleaned, None),
    };
    let separator = size.chars().find(|c| matches!(c, 'x' | 'X' | '*'))?;
    let (w, h) = size.split_once(separator)?;
    let width: u32 = w.parse().ok()?;
    let height: u32 = h.parse().ok()?;
    if !(16..=16384).contains(&width) || !(16..=16384).contains(&height) {
        return None;
    }
    Some((width, height, fps))
}

/// Shows the finished file in Explorer.
fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer wants /select,"path" as one argument and mangles anything
        // else, so the quoting is written out by hand rather than escaped.
        let mut command = std::process::Command::new("explorer.exe");
        command.raw_arg(format!("/select,\"{}\"", path.display()));
        let _ = command.spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(folder) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_specs() {
        assert_eq!(parse_target_spec("1920x1080@30"), Some((1920, 1080, Some(30.0))));
        assert_eq!(parse_target_spec(" 1080 X 1920 "), Some((1080, 1920, None)));
        assert_eq!(parse_target_spec("640*480@23.976"), Some((640, 480, Some(23.976))));
        assert_eq!(parse_target_spec("1920"), None);
        assert_eq!(parse_target_spec("4x4"), None, "absurdly small is a typo");
        assert_eq!(parse_target_spec("1920x1080@600"), None, "600 fps is a typo");
    }
}
