use std::{ops::Range, pin::pin, time::Duration};

use anyhow::{Context as _, Result};
use collections::HashSet;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent, EditorSettings, actions::SelectAll};
use file_icons::FileIcons;
use futures::StreamExt as _;
use gpui::{
    Action, App, AsyncWindowContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, HighlightStyle, PromptLevel, ScrollStrategy, StrikethroughStyle, StyledText,
    Subscription, Task, TaskExt as _, UniformListScrollHandle, WeakEntity, Window, actions,
    uniform_list,
};
use language::Buffer;
use project::{
    Project, ProjectPath,
    search::{SearchQuery, SearchResult},
};
use settings::{SeedQuerySetting, Settings as _};
use text::{BufferId, Point, ToPoint as _};
use ui::{IconButtonShape, ListItem, Tooltip, WithScrollbar as _, prelude::*};
use util::{ResultExt as _, paths::PathMatcher};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotifyTaskExt as _,
    searchable::SearchableItem as _,
};

use crate::{
    EXCLUDE_PLACEHOLDER, INCLUDE_PLACEHOLDER, ReplaceAll, SearchOption, SearchOptions,
    ToggleCaseSensitive, ToggleIncludeIgnored, ToggleRegex, ToggleReplace, ToggleWholeWord,
    project_search::split_glob_patterns,
    search_bar::{input_base_styles, render_text_input},
};

const SEARCH_PANEL_KEY: &str = "SearchPanel";
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// How many bytes of the line are kept before a match in the preview, so that the
/// match itself stays visible in a narrow panel.
const MAX_PREVIEW_PREFIX_LEN: usize = 30;
const MAX_PREVIEW_LEN: usize = 250;

actions!(
    search_panel,
    [
        /// Toggles focus on the search panel.
        ToggleFocus,
        /// Opens the search panel and focuses its query input, seeded with the current selection.
        Deploy,
        /// Opens the search panel with replace enabled.
        DeployReplace,
        /// Opens the current search results in a project search tab.
        OpenInEditor,
        /// Collapses all files in the results, or expands them when all are collapsed.
        ToggleCollapseAll,
        /// Clears the query and the search results.
        ClearSearchResults,
        /// Expands the selected file in the results.
        ExpandSelectedEntry,
        /// Collapses the selected file in the results.
        CollapseSelectedEntry,
        /// Removes the selected match or file from the results.
        DismissSelected,
        /// Replaces the selected match, or all matches in the selected file.
        ReplaceSelected,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<SearchPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Deploy, window, cx| {
            SearchPanel::deploy(workspace, false, window, cx);
        });
        workspace.register_action(|workspace, _: &DeployReplace, window, cx| {
            SearchPanel::deploy(workspace, true, window, cx);
        });
    })
    .detach();
}

struct FileResult {
    buffer: Entity<Buffer>,
    buffer_id: BufferId,
    project_path: ProjectPath,
    file_name: SharedString,
    directory: SharedString,
    icon: Option<SharedString>,
    matches: Vec<Range<text::Anchor>>,
    collapsed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    File {
        file_index: usize,
    },
    Match {
        file_index: usize,
        match_index: usize,
    },
}

/// Identifies the selected row independently of its position, since rows shift as results
/// stream in sorted order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    buffer_id: BufferId,
    match_index: Option<usize>,
}

pub struct SearchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    query_editor: Entity<Editor>,
    replacement_editor: Entity<Editor>,
    include_editor: Entity<Editor>,
    exclude_editor: Entity<Editor>,
    search_options: SearchOptions,
    replace_enabled: bool,
    filters_enabled: bool,
    position: DockPosition,
    file_results: Vec<FileResult>,
    visible_rows: Vec<Row>,
    selection: Option<Selection>,
    active_query: Option<SearchQuery>,
    query_error: Option<SharedString>,
    limit_reached: bool,
    searching: bool,
    scroll_handle: UniformListScrollHandle,
    pending_debounce: Task<()>,
    pending_search: Option<Task<()>>,
    pending_serialization: Task<Option<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let serialization_key = workspace
            .read_with(&cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten();
        let position = match serialization_key {
            Some(serialization_key) => {
                let kvp = cx.update(|_, cx| KeyValueStore::global(cx))?;
                cx.background_spawn(async move { kvp.read_kvp(&serialization_key) })
                    .await
                    .context("loading search panel")
                    .log_err()
                    .flatten()
                    .and_then(|position| deserialize_position(&position))
            }
            None => None,
        };
        workspace.update_in(&mut cx, |workspace, window, cx| {
            Self::new(
                workspace,
                position.unwrap_or(DockPosition::Left),
                window,
                cx,
            )
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        position: DockPosition,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();
        cx.new(|cx| {
            let query_editor = Self::create_input("Search", window, cx);
            let replacement_editor = Self::create_input("Replace", window, cx);
            let include_editor = Self::create_input(INCLUDE_PLACEHOLDER, window, cx);
            let exclude_editor = Self::create_input(EXCLUDE_PLACEHOLDER, window, cx);
            let subscriptions = vec![
                cx.subscribe(&query_editor, Self::on_search_input_event),
                cx.subscribe(&include_editor, Self::on_search_input_event),
                cx.subscribe(&exclude_editor, Self::on_search_input_event),
                cx.subscribe(&replacement_editor, |_, _, event: &EditorEvent, cx| {
                    if let EditorEvent::BufferEdited = event {
                        cx.notify();
                    }
                }),
            ];
            Self {
                workspace: workspace_handle,
                project,
                focus_handle: cx.focus_handle(),
                query_editor,
                replacement_editor,
                include_editor,
                exclude_editor,
                search_options: SearchOptions::from_settings(
                    &EditorSettings::get_global(cx).search,
                ),
                replace_enabled: false,
                filters_enabled: false,
                position,
                file_results: Vec::new(),
                visible_rows: Vec::new(),
                selection: None,
                active_query: None,
                query_error: None,
                limit_reached: false,
                searching: false,
                scroll_handle: UniformListScrollHandle::new(),
                pending_debounce: Task::ready(()),
                pending_search: None,
                pending_serialization: Task::ready(None),
                _subscriptions: subscriptions,
            }
        })
    }

    fn create_input(
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(placeholder, window, cx);
            editor
        })
    }

    fn deploy(
        workspace: &mut Workspace,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let seed = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .map(|editor| {
                editor.update(cx, |editor, cx| {
                    editor.query_suggestion(Some(SeedQuerySetting::Selection), window, cx)
                })
            })
            .filter(|seed| !seed.is_empty());
        let Some(panel) = workspace.focus_panel::<SearchPanel>(window, cx) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            if replace {
                panel.replace_enabled = true;
            }
            panel.query_editor.update(cx, |editor, cx| {
                if let Some(seed) = seed {
                    editor.set_text(seed, window, cx);
                }
                editor.select_all(&SelectAll, window, cx);
            });
            window.focus(&panel.query_editor.focus_handle(cx), cx);
            cx.notify();
        });
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{SEARCH_PANEL_KEY}-{id:?}"))
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| Self::serialization_key(workspace))
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        let position = serialize_position(self.position).to_string();
        self.pending_serialization = cx.background_spawn(async move {
            kvp.write_kvp(serialization_key, position).await.log_err()
        });
    }

    fn on_search_input_event(
        &mut self,
        _: Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if let EditorEvent::BufferEdited = event {
            self.schedule_search(cx);
        }
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.pending_debounce = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.search(cx)).log_err();
        });
    }

    fn build_query(&self, cx: &App) -> Result<Option<SearchQuery>> {
        let text = self.query_editor.read(cx).text(cx);
        if text.is_empty() {
            return Ok(None);
        }
        let (files_to_include, files_to_exclude) = if self.filters_enabled {
            (
                self.parse_path_matcher(&self.include_editor, cx)?,
                self.parse_path_matcher(&self.exclude_editor, cx)?,
            )
        } else {
            (PathMatcher::default(), PathMatcher::default())
        };
        // Patterns are matched against full paths when several worktrees are open, so that
        // they can be told apart, mirroring the project search.
        let match_full_paths = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let query = self.search_options.build_query(
            text,
            files_to_include,
            files_to_exclude,
            match_full_paths,
            None,
        )?;
        Ok((!query.is_empty()).then_some(query))
    }

    fn parse_path_matcher(&self, editor: &Entity<Editor>, cx: &App) -> Result<PathMatcher> {
        let text = editor.read(cx).text(cx);
        let globs = split_glob_patterns(&text)
            .into_iter()
            .map(str::trim)
            .filter(|glob| !glob.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        Ok(PathMatcher::new(
            &globs,
            self.project.read(cx).path_style(cx),
        )?)
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        self.pending_debounce = Task::ready(());
        self.pending_search = None;
        self.reset_results();
        let query = match self.build_query(cx) {
            Ok(Some(query)) => query,
            Ok(None) => {
                cx.notify();
                return;
            }
            Err(error) => {
                self.query_error = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        self.active_query = Some(query.clone());
        self.searching = true;
        let search = self
            .project
            .update(cx, |project, cx| project.search(query, cx));
        self.pending_search = Some(cx.spawn(async move |this, cx| {
            // The search runs only as long as its task handle is alive.
            let _task_handle = search.task_handle;
            let mut chunks = pin!(search.rx.ready_chunks(256));
            while let Some(results) = chunks.next().await {
                if this
                    .update(cx, |this, cx| this.append_results(results, cx))
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.searching = false;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn reset_results(&mut self) {
        self.file_results.clear();
        self.visible_rows.clear();
        self.selection = None;
        self.active_query = None;
        self.query_error = None;
        self.limit_reached = false;
        self.searching = false;
    }

    fn append_results(&mut self, results: Vec<SearchResult>, cx: &mut Context<Self>) {
        let include_root = self.project.read(cx).visible_worktrees(cx).count() > 1;
        for result in results {
            match result {
                SearchResult::Buffer { buffer, ranges } => {
                    if ranges.is_empty() {
                        continue;
                    }
                    let Some(file_result) = FileResult::new(buffer, ranges, include_root, cx)
                    else {
                        continue;
                    };
                    match self.file_results.binary_search_by(|existing| {
                        existing.sort_key().cmp(&file_result.sort_key())
                    }) {
                        Ok(index) => self.file_results[index] = file_result,
                        Err(index) => self.file_results.insert(index, file_result),
                    }
                }
                SearchResult::LimitReached => self.limit_reached = true,
                SearchResult::WaitingForScan | SearchResult::Searching => {}
            }
        }
        self.rebuild_visible_rows();
        cx.notify();
    }

    fn rebuild_visible_rows(&mut self) {
        self.file_results.retain(|file| !file.matches.is_empty());
        self.visible_rows.clear();
        for (file_index, file) in self.file_results.iter().enumerate() {
            self.visible_rows.push(Row::File { file_index });
            if !file.collapsed {
                self.visible_rows
                    .extend((0..file.matches.len()).map(|match_index| Row::Match {
                        file_index,
                        match_index,
                    }));
            }
        }
        if let Some(selection) = self.selection
            && self.row_index_for_selection(selection).is_none()
        {
            self.selection = None;
        }
    }

    fn match_count(&self) -> usize {
        self.file_results
            .iter()
            .map(|file| file.matches.len())
            .sum()
    }

    fn selection_for_row(&self, row: Row) -> Option<Selection> {
        match row {
            Row::File { file_index } => Some(Selection {
                buffer_id: self.file_results.get(file_index)?.buffer_id,
                match_index: None,
            }),
            Row::Match {
                file_index,
                match_index,
            } => Some(Selection {
                buffer_id: self.file_results.get(file_index)?.buffer_id,
                match_index: Some(match_index),
            }),
        }
    }

    fn row_index_for_selection(&self, selection: Selection) -> Option<usize> {
        self.visible_rows
            .iter()
            .position(|row| self.selection_for_row(*row) == Some(selection))
    }

    fn selected_row(&self) -> Option<(usize, Row)> {
        let index = self.row_index_for_selection(self.selection?)?;
        Some((index, *self.visible_rows.get(index)?))
    }

    fn select_row(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.visible_rows.get(index).copied() else {
            return;
        };
        self.selection = self.selection_for_row(row);
        self.scroll_handle
            .scroll_to_item(index, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next = match self.selected_row() {
            Some((index, _)) => index + 1,
            None => 0,
        };
        self.select_row(next.min(self.visible_rows.len().saturating_sub(1)), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self
            .selected_row()
            .map_or(0, |(index, _)| index.saturating_sub(1));
        self.select_row(previous, cx);
    }

    fn select_first(&mut self, _: &menu::SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select_row(self.visible_rows.len().saturating_sub(1), cx);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_selected(false, window, cx);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_selected(true, window, cx);
    }

    fn activate_selected(
        &mut self,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_row() {
            Some((_, Row::File { file_index })) => self.toggle_file_collapsed(file_index, cx),
            Some((
                _,
                Row::Match {
                    file_index,
                    match_index,
                },
            )) => self.open_match(file_index, match_index, focus_editor, window, cx),
            None => {}
        }
    }

    fn set_selected_file_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let Some((_, row)) = self.selected_row() else {
            return;
        };
        let file_index = match row {
            Row::File { file_index } | Row::Match { file_index, .. } => file_index,
        };
        let Some(file) = self.file_results.get_mut(file_index) else {
            return;
        };
        file.collapsed = collapsed;
        if collapsed {
            self.selection = Some(Selection {
                buffer_id: file.buffer_id,
                match_index: None,
            });
        }
        self.rebuild_visible_rows();
        if let Some(index) = self
            .selection
            .and_then(|selection| self.row_index_for_selection(selection))
        {
            self.select_row(index, cx);
        }
        cx.notify();
    }

    fn expand_selected_entry(
        &mut self,
        _: &ExpandSelectedEntry,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selected_file_collapsed(false, cx);
    }

    fn collapse_selected_entry(
        &mut self,
        _: &CollapseSelectedEntry,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selected_file_collapsed(true, cx);
    }

    fn toggle_file_collapsed(&mut self, file_index: usize, cx: &mut Context<Self>) {
        if let Some(file) = self.file_results.get_mut(file_index) {
            file.collapsed = !file.collapsed;
            self.rebuild_visible_rows();
            cx.notify();
        }
    }

    fn toggle_collapse_all(
        &mut self,
        _: &ToggleCollapseAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let collapse = self.file_results.iter().any(|file| !file.collapsed);
        for file in &mut self.file_results {
            file.collapsed = collapse;
        }
        if collapse && let Some(selection) = self.selection.as_mut() {
            selection.match_index = None;
        }
        self.rebuild_visible_rows();
        cx.notify();
    }

    fn clear_search_results(
        &mut self,
        _: &ClearSearchResults,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query_editor
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.search(cx);
        window.focus(&self.query_editor.focus_handle(cx), cx);
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut Context<Self>) {
        self.search_options.toggle(option);
        self.search(cx);
    }

    fn toggle_replace(&mut self, _: &ToggleReplace, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_enabled = !self.replace_enabled;
        let editor = if self.replace_enabled {
            &self.replacement_editor
        } else {
            &self.query_editor
        };
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
    }

    fn toggle_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filters_enabled = !self.filters_enabled;
        if self.filters_enabled {
            window.focus(&self.include_editor.focus_handle(cx), cx);
        }
        let has_filters = !self.include_editor.read(cx).text(cx).is_empty()
            || !self.exclude_editor.read(cx).text(cx).is_empty();
        if has_filters {
            self.search(cx);
        }
        cx.notify();
    }

    fn open_in_editor(&mut self, _: &OpenInEditor, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query_editor.read(cx).text(cx);
        if query.is_empty() {
            return;
        }
        let filter_text = |editor: &Entity<Editor>| {
            Some(editor.read(cx).text(cx)).filter(|text| self.filters_enabled && !text.is_empty())
        };
        let action = workspace::DeploySearch {
            replace_enabled: self.replace_enabled,
            included_files: filter_text(&self.include_editor),
            excluded_files: filter_text(&self.exclude_editor),
            query: Some(query),
            regex: Some(self.search_options.contains(SearchOptions::REGEX)),
            case_sensitive: Some(self.search_options.contains(SearchOptions::CASE_SENSITIVE)),
            whole_word: Some(self.search_options.contains(SearchOptions::WHOLE_WORD)),
            ..Default::default()
        };
        window.dispatch_action(action.boxed_clone(), cx);
    }

    fn open_match(
        &mut self,
        file_index: usize,
        match_index: usize,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.file_results.get(file_index) else {
            return;
        };
        let Some(range) = file.matches.get(match_index) else {
            return;
        };
        let buffer = file.buffer.read(cx);
        let target = range.start.to_point(buffer)..range.end.to_point(buffer);
        let path = file.project_path.clone();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open_task = workspace.update(cx, |workspace, cx| {
            workspace.open_path_preview(path, None, focus_editor, true, true, window, cx)
        });
        cx.spawn_in(window, async move |_, cx| {
            let item = open_task.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.downgrade().update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_range(target, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn dismiss_selected(&mut self, _: &DismissSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some((index, row)) = self.selected_row() else {
            return;
        };
        self.dismiss_row(row, cx);
        self.select_row(index.min(self.visible_rows.len().saturating_sub(1)), cx);
    }

    fn dismiss_row(&mut self, row: Row, cx: &mut Context<Self>) {
        match row {
            Row::File { file_index } => {
                if file_index < self.file_results.len() {
                    self.file_results.remove(file_index);
                }
            }
            Row::Match {
                file_index,
                match_index,
            } => {
                if let Some(file) = self.file_results.get_mut(file_index)
                    && match_index < file.matches.len()
                {
                    file.matches.remove(match_index);
                }
            }
        }
        self.rebuild_visible_rows();
        cx.notify();
    }

    fn replacement_query(&self, cx: &App) -> Option<SearchQuery> {
        if !self.replace_enabled {
            return None;
        }
        let replacement = self.replacement_editor.read(cx).text(cx);
        Some(self.active_query.clone()?.with_replacement(replacement))
    }

    fn replace_selected(
        &mut self,
        _: &ReplaceSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, row)) = self.selected_row() else {
            return;
        };
        self.replace_row(row, window, cx);
        self.select_row(index.min(self.visible_rows.len().saturating_sub(1)), cx);
    }

    fn replace_row(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let (file_index, match_indices) = match row {
            Row::File { file_index } => (file_index, None),
            Row::Match {
                file_index,
                match_index,
            } => (file_index, Some(vec![match_index])),
        };
        let buffers = self
            .replace_in_file(file_index, match_indices, cx)
            .into_iter()
            .collect();
        self.rebuild_visible_rows();
        self.save_buffers(buffers, window, cx);
        cx.notify();
    }

    fn replace_all(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replace_enabled || self.file_results.is_empty() {
            return;
        }
        let file_count = self.file_results.len();
        if file_count == 1 {
            self.replace_all_confirmed(window, cx);
            return;
        }
        let match_count = self.match_count();
        let replacement = self.replacement_editor.read(cx).text(cx);
        let message = format!(
            "Replace {match_count} occurrences across {file_count} files with \"{replacement}\"?"
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Replace", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? == 0 {
                this.update_in(cx, |this, window, cx| {
                    this.replace_all_confirmed(window, cx)
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn replace_all_confirmed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let buffers = (0..self.file_results.len())
            .filter_map(|file_index| self.replace_in_file(file_index, None, cx))
            .collect();
        self.rebuild_visible_rows();
        self.save_buffers(buffers, window, cx);
        cx.notify();
    }

    /// Replaces the given matches in a file (all of them when `match_indices` is `None`) and
    /// drops them from the results. Returns the buffer when it should be saved, i.e. when it had
    /// no unsaved changes of its own before the replacement.
    fn replace_in_file(
        &mut self,
        file_index: usize,
        match_indices: Option<Vec<usize>>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Buffer>> {
        let query = self.replacement_query(cx)?;
        let file = self.file_results.get_mut(file_index)?;
        let mut match_indices = match_indices.unwrap_or_else(|| (0..file.matches.len()).collect());
        match_indices.retain(|index| *index < file.matches.len());
        match_indices.sort_unstable();
        match_indices.dedup();
        if match_indices.is_empty() {
            return None;
        }

        let buffer = file.buffer.read(cx);
        let was_dirty = buffer.is_dirty();
        let edits = match_indices
            .iter()
            .filter_map(|index| {
                let range = file.matches.get(*index)?;
                let replacement = replacement_for_match(&query, buffer, range)?;
                Some((range.clone(), replacement))
            })
            .collect::<Vec<_>>();
        for index in match_indices.iter().rev() {
            file.matches.remove(*index);
        }
        if edits.is_empty() {
            return None;
        }
        let buffer = file.buffer.clone();
        buffer.update(cx, |buffer, cx| {
            buffer.edit(edits, None, cx);
        });
        (!was_dirty).then_some(buffer)
    }

    // Files that aren't open in an editor would otherwise keep their edits only in memory, where
    // they're easy to lose.
    fn save_buffers(
        &mut self,
        buffers: HashSet<Entity<Buffer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if buffers.is_empty() {
            return;
        }
        self.project
            .update(cx, |project, cx| project.save_buffers(buffers, cx))
            .detach_and_notify_err(self.workspace.clone(), window, cx);
    }

    fn status_text(&self) -> (SharedString, Color) {
        if let Some(error) = &self.query_error {
            return (error.clone(), Color::Error);
        }
        let match_count = self.match_count();
        let file_count = self.file_results.len();
        if self.active_query.is_none() {
            return (SharedString::default(), Color::Muted);
        }
        if match_count == 0 {
            let text = if self.searching {
                "Searching…"
            } else {
                "No results found"
            };
            return (text.into(), Color::Muted);
        }
        let results = if match_count == 1 {
            "result"
        } else {
            "results"
        };
        let files = if file_count == 1 { "file" } else { "files" };
        let mut text = format!("{match_count} {results} in {file_count} {files}");
        if self.limit_reached {
            text.push_str(" (limit reached, refine your search)");
        }
        (text.into(), Color::Muted)
    }

    fn render_option_button(
        &self,
        option: SearchOption,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let options = option.as_options();
        let label = option.label();
        let action = option.to_toggle_action();
        let focus_handle = self.query_editor.focus_handle(cx);
        IconButton::new(
            ("search-panel-option", options.bits() as u64),
            option.icon(),
        )
        .shape(IconButtonShape::Square)
        .icon_size(IconSize::Small)
        .toggle_state(self.search_options.contains(options))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.toggle_search_option(options, cx);
        }))
        .tooltip(move |_, cx| Tooltip::for_action_in(label, action, &focus_handle, cx))
    }

    fn render_inputs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let border = colors.border;
        let query_border = if self.query_error.is_some() {
            Color::Error.color(cx)
        } else {
            border
        };
        let focus_handle = self.query_editor.focus_handle(cx);
        let has_results = !self.file_results.is_empty();

        h_flex()
            .w_full()
            .p_2()
            .gap_1()
            .items_start()
            .child(
                IconButton::new(
                    "search-panel-toggle-replace",
                    if self.replace_enabled {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    },
                )
                .shape(IconButtonShape::Square)
                .icon_size(IconSize::Small)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.toggle_replace(&ToggleReplace, window, cx);
                }))
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |_, cx| {
                        Tooltip::for_action_in("Toggle Replace", &ToggleReplace, &focus_handle, cx)
                    }
                }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        input_base_styles(query_border, |div| div.w_full())
                            .child(div().flex_1().min_w_0().child(render_text_input(
                                &self.query_editor,
                                None,
                                cx,
                            )))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .child(
                                        self.render_option_button(SearchOption::CaseSensitive, cx),
                                    )
                                    .child(self.render_option_button(SearchOption::WholeWord, cx))
                                    .child(self.render_option_button(SearchOption::Regex, cx)),
                            ),
                    )
                    .when(self.replace_enabled, |this| {
                        this.child(
                            input_base_styles(border, |div| div.w_full())
                                .child(div().flex_1().min_w_0().child(render_text_input(
                                    &self.replacement_editor,
                                    None,
                                    cx,
                                )))
                                .child(
                                    IconButton::new(
                                        "search-panel-replace-all",
                                        IconName::ReplaceAll,
                                    )
                                    .shape(IconButtonShape::Square)
                                    .icon_size(IconSize::Small)
                                    .disabled(!has_results)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.replace_all(&ReplaceAll, window, cx);
                                    }))
                                    .tooltip({
                                        let focus_handle = focus_handle.clone();
                                        move |_, cx| {
                                            Tooltip::for_action_in(
                                                "Replace All",
                                                &ReplaceAll,
                                                &focus_handle,
                                                cx,
                                            )
                                        }
                                    }),
                                ),
                        )
                    })
                    .child(
                        h_flex().justify_end().child(
                            IconButton::new("search-panel-toggle-filters", IconName::Ellipsis)
                                .shape(IconButtonShape::Square)
                                .icon_size(IconSize::Small)
                                .toggle_state(self.filters_enabled)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.toggle_filters(window, cx);
                                }))
                                .tooltip(Tooltip::text("Toggle Search Details")),
                        ),
                    )
                    .when(self.filters_enabled, |this| {
                        this.child(
                            Label::new("files to include")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(input_base_styles(border, |div| div.w_full()).child(
                            div().flex_1().min_w_0().child(render_text_input(
                                &self.include_editor,
                                None,
                                cx,
                            )),
                        ))
                        .child(
                            Label::new("files to exclude")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            input_base_styles(border, |div| div.w_full())
                                .child(div().flex_1().min_w_0().child(render_text_input(
                                    &self.exclude_editor,
                                    None,
                                    cx,
                                )))
                                .child(self.render_option_button(SearchOption::IncludeIgnored, cx)),
                        )
                    }),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, color) = self.status_text();
        let has_query = self.active_query.is_some();
        let all_collapsed = self.file_results.iter().all(|file| file.collapsed);
        h_flex()
            .w_full()
            .px_2()
            .pb_1()
            .gap_1()
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(status)
                        .size(LabelSize::Small)
                        .color(color)
                        .truncate(),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_0p5()
                    .child(
                        IconButton::new("search-panel-open-in-editor", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .disabled(!has_query)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_in_editor(&OpenInEditor, window, cx);
                            }))
                            .tooltip(Tooltip::text("Open in Editor")),
                    )
                    .child(
                        IconButton::new(
                            "search-panel-collapse-all",
                            if all_collapsed && !self.file_results.is_empty() {
                                IconName::ExpandVertical
                            } else {
                                IconName::ListCollapse
                            },
                        )
                        .icon_size(IconSize::Small)
                        .disabled(self.file_results.is_empty())
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.toggle_collapse_all(&ToggleCollapseAll, window, cx);
                        }))
                        .tooltip(Tooltip::text(if all_collapsed {
                            "Expand All"
                        } else {
                            "Collapse All"
                        })),
                    )
                    .child(
                        IconButton::new("search-panel-clear", IconName::Close)
                            .icon_size(IconSize::Small)
                            .disabled(!has_query)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.clear_search_results(&ClearSearchResults, window, cx);
                            }))
                            .tooltip(Tooltip::text("Clear Search Results")),
                    ),
            )
    }

    fn render_row(
        &self,
        index: usize,
        replacement_query: Option<&SearchQuery>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let row = *self.visible_rows.get(index)?;
        let selected = self.selection.is_some() && self.selection_for_row(row) == self.selection;
        match row {
            Row::File { file_index } => {
                let file = self.file_results.get(file_index)?;
                Some(self.render_file_row(index, file_index, file, selected, cx))
            }
            Row::Match {
                file_index,
                match_index,
            } => {
                let file = self.file_results.get(file_index)?;
                let range = file.matches.get(match_index)?;
                Some(self.render_match_row(
                    index,
                    row,
                    file,
                    range,
                    replacement_query,
                    selected,
                    cx,
                ))
            }
        }
    }

    fn render_row_actions(&self, row: Row, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_0p5()
            .when(self.replace_enabled, |this| {
                this.child(
                    IconButton::new("replace", IconName::Replace)
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            cx.stop_propagation();
                            this.replace_row(row, window, cx);
                        }))
                        .tooltip(Tooltip::text(match row {
                            Row::File { .. } => "Replace All in File",
                            Row::Match { .. } => "Replace",
                        })),
                )
            })
            .child(
                IconButton::new("dismiss", IconName::Close)
                    .icon_size(IconSize::Small)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.dismiss_row(row, cx);
                    }))
                    .tooltip(Tooltip::text("Dismiss")),
            )
    }

    fn render_file_row(
        &self,
        index: usize,
        file_index: usize,
        file: &FileResult,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row = Row::File { file_index };
        ListItem::new(("search-panel-file", index))
            .toggle_state(selected)
            .toggle(!file.collapsed)
            .always_show_disclosure_icon(true)
            .on_toggle(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_file_collapsed(file_index, cx);
            }))
            .start_slot::<Icon>(file.icon.clone().map(|icon| {
                Icon::from_path(icon)
                    .color(Color::Muted)
                    .size(IconSize::Small)
            }))
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Label::new(file.file_name.clone()).single_line())
                    .child(
                        Label::new(file.directory.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .end_slot(
                Label::new(file.matches.len().to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .end_slot_on_hover(self.render_row_actions(row, cx))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.selection = this.selection_for_row(row);
                this.toggle_file_collapsed(file_index, cx);
                window.focus(&this.focus_handle, cx);
            }))
            .into_any_element()
    }

    fn render_match_row(
        &self,
        index: usize,
        row: Row,
        file: &FileResult,
        range: &Range<text::Anchor>,
        replacement_query: Option<&SearchQuery>,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let buffer = file.buffer.read(cx);
        let start = range.start.to_point(buffer);
        let end = range.end.to_point(buffer);
        let line = line_text(buffer, start.row);
        let hit_end = if end.row == start.row {
            end.column as usize
        } else {
            line.len()
        };
        let (mut text, hit) = line_preview(&line, start.column as usize..hit_end);
        let replacement = replacement_query
            .and_then(|query| replacement_for_match(query, buffer, range))
            .map(|replacement| replacement.replace('\n', "↵"));

        let theme = cx.theme();
        let mut highlights = Vec::new();
        match replacement {
            Some(replacement) => {
                let replacement_range = hit.end..hit.end + replacement.len();
                text.insert_str(hit.end, &replacement);
                highlights.push((
                    hit,
                    HighlightStyle {
                        background_color: Some(theme.status().deleted_background),
                        strikethrough: Some(StrikethroughStyle {
                            thickness: px(1.),
                            color: None,
                        }),
                        ..HighlightStyle::default()
                    },
                ));
                highlights.push((
                    replacement_range,
                    HighlightStyle {
                        background_color: Some(theme.status().created_background),
                        ..HighlightStyle::default()
                    },
                ));
            }
            None => highlights.push((
                hit,
                HighlightStyle {
                    background_color: Some(theme.colors().search_match_background),
                    ..HighlightStyle::default()
                },
            )),
        }
        let Row::Match {
            file_index,
            match_index,
        } = row
        else {
            return div().into_any_element();
        };

        ListItem::new(("search-panel-match", index))
            .indent_level(1)
            .indent_step_size(px(20.))
            .toggle_state(selected)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_ui_sm(cx)
                    .child(StyledText::new(text).with_highlights(highlights)),
            )
            .end_slot_on_hover(self.render_row_actions(row, cx))
            .tooltip(Tooltip::text(format!("Line {}", start.row + 1)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.selection = this.selection_for_row(row);
                let focus_editor = event.click_count() > 1;
                if !focus_editor {
                    window.focus(&this.focus_handle, cx);
                }
                this.open_match(file_index, match_index, focus_editor, window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_results(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.visible_rows.is_empty() {
            return div().flex_1().into_any_element();
        }
        v_flex()
            .id("search-panel-results")
            .key_context("SearchResults")
            .track_focus(&self.focus_handle)
            .flex_1()
            .min_h_0()
            .w_full()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::secondary_confirm))
            .on_action(cx.listener(Self::expand_selected_entry))
            .on_action(cx.listener(Self::collapse_selected_entry))
            .on_action(cx.listener(Self::dismiss_selected))
            .on_action(cx.listener(Self::replace_selected))
            .child(
                uniform_list(
                    "search-panel-rows",
                    self.visible_rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let replacement_query = this.replacement_query(cx);
                        range
                            .filter_map(|index| {
                                this.render_row(index, replacement_query.as_ref(), cx)
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&self.scroll_handle),
            )
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
            .into_any_element()
    }
}

impl FileResult {
    fn new(
        buffer: Entity<Buffer>,
        matches: Vec<Range<text::Anchor>>,
        include_root: bool,
        cx: &App,
    ) -> Option<Self> {
        let buffer_ref = buffer.read(cx);
        let file = buffer_ref.file()?;
        let path = file.path().clone();
        let path_style = file.path_style(cx);
        let file_name = SharedString::from(path.file_name()?.to_string());
        let directory = if include_root {
            file.full_path(cx)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            path.parent()
                .map(|parent| parent.display(path_style).into_owned())
                .unwrap_or_default()
        };
        let icon = FileIcons::get_icon(path.as_std_path(), cx);
        Some(Self {
            buffer_id: buffer_ref.remote_id(),
            project_path: ProjectPath {
                worktree_id: file.worktree_id(cx),
                path,
            },
            buffer,
            file_name,
            directory: directory.into(),
            icon,
            matches,
            collapsed: false,
        })
    }

    fn sort_key(&self) -> &ProjectPath {
        &self.project_path
    }
}

fn line_text(buffer: &text::BufferSnapshot, row: u32) -> String {
    buffer
        .text_for_range(Point::new(row, 0)..Point::new(row, buffer.line_len(row)))
        .collect()
}

fn replacement_for_match(
    query: &SearchQuery,
    buffer: &text::BufferSnapshot,
    range: &Range<text::Anchor>,
) -> Option<String> {
    let start = range.start.to_point(buffer);
    let end = range.end.to_point(buffer);
    // Regex replacements are expanded against the whole line so that lookarounds keep their
    // context, the same way editor replacements do.
    let replacement = if start.row == end.row {
        let line = line_text(buffer, start.row);
        query
            .replacement_for(&line, start.column as usize..end.column as usize)?
            .into_owned()
    } else {
        let text = buffer.text_for_range(range.clone()).collect::<String>();
        query.replacement_for(&text, 0..text.len())?.into_owned()
    };
    Some(replacement)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Returns a single-line preview of `line` that keeps the match `hit` visible, along with the
/// position of the match within the preview.
fn line_preview(line: &str, hit: Range<usize>) -> (String, Range<usize>) {
    let hit_start = floor_char_boundary(line, hit.start);
    let hit_end = floor_char_boundary(line, hit.end.max(hit_start));
    let content_start = line.len() - line.trim_start().len();
    let mut start = content_start.min(hit_start);
    let mut prefix = "";
    if hit_start - start > MAX_PREVIEW_PREFIX_LEN {
        start = floor_char_boundary(line, hit_start - MAX_PREVIEW_PREFIX_LEN);
        prefix = "…";
    }
    let end = floor_char_boundary(line, (start + MAX_PREVIEW_LEN).max(hit_end));
    let preview = format!("{prefix}{}", &line[start..end]);
    let offset = prefix.len();
    (
        preview,
        hit_start - start + offset..hit_end - start + offset,
    )
}

fn serialize_position(position: DockPosition) -> &'static str {
    match position {
        DockPosition::Left => "left",
        DockPosition::Bottom => "bottom",
        DockPosition::Right => "right",
    }
}

fn deserialize_position(position: &str) -> Option<DockPosition> {
    match position {
        "left" => Some(DockPosition::Left),
        "bottom" => Some(DockPosition::Bottom),
        "right" => Some(DockPosition::Right),
        _ => None,
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SearchPanel {}

impl Panel for SearchPanel {
    fn persistent_name() -> &'static str {
        "Search Panel"
    }

    fn panel_key() -> &'static str {
        SEARCH_PANEL_KEY
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.query_editor.focus_handle(cx)
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        self.serialize(cx);
        // Docks re-check their panels' positions when settings change; the position of this
        // panel isn't stored in settings, so nudge them explicitly.
        cx.update_global::<settings::SettingsStore, _>(|_, _| {});
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(300.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::MagnifyingGlass)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Search Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        4
    }
}

impl Render for SearchPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("search-panel")
            .key_context("SearchPanel")
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().colors().panel_background)
            .on_action(cx.listener(|this, _: &ToggleCaseSensitive, _, cx| {
                this.toggle_search_option(SearchOptions::CASE_SENSITIVE, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleWholeWord, _, cx| {
                this.toggle_search_option(SearchOptions::WHOLE_WORD, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleRegex, _, cx| {
                this.toggle_search_option(SearchOptions::REGEX, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleIncludeIgnored, _, cx| {
                this.toggle_search_option(SearchOptions::INCLUDE_IGNORED, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| this.search(cx)))
            .on_action(cx.listener(Self::toggle_replace))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::open_in_editor))
            .on_action(cx.listener(Self::toggle_collapse_all))
            .on_action(cx.listener(Self::clear_search_results))
            .child(self.render_inputs(cx))
            .child(self.render_status(cx))
            .child(self.render_results(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Fs as _};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    async fn build_panel(
        files: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (
        Entity<SearchPanel>,
        Entity<Project>,
        Arc<FakeFs>,
        VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(path!("/dir"), files).await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            SearchPanel::new(workspace, DockPosition::Left, window, cx)
        });
        (panel, project, fs, cx)
    }

    fn search(panel: &Entity<SearchPanel>, query: &str, cx: &mut VisualTestContext) {
        panel.update_in(cx, |panel, window, cx| {
            panel
                .query_editor
                .update(cx, |editor, cx| editor.set_text(query, window, cx));
            panel.search(cx);
        });
        cx.run_until_parked();
    }

    fn results(panel: &Entity<SearchPanel>, cx: &mut VisualTestContext) -> Vec<(String, usize)> {
        panel.read_with(cx, |panel, _| {
            panel
                .file_results
                .iter()
                .map(|file| {
                    (
                        file.project_path.path.as_unix_str().to_string(),
                        file.matches.len(),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn test_line_preview() {
        assert_eq!(
            line_preview("    let foo = 1;", 8..11),
            ("let foo = 1;".to_string(), 4..7)
        );
        let long_prefix = format!("{}needle", "x".repeat(100));
        let (preview, hit) = line_preview(&long_prefix, 100..106);
        assert!(preview.starts_with('…'));
        assert_eq!(&preview[hit], "needle");
        let (preview, hit) = line_preview("ééééé needle", 11..17);
        assert_eq!(&preview[hit], "needle");
    }

    #[gpui::test]
    async fn test_search_groups_results_by_file(cx: &mut TestAppContext) {
        let (panel, _, _, mut cx) = build_panel(
            json!({
                "b.rs": "fn needle() {}\nneedle();\n",
                "a.rs": "// needle\n",
                "c.rs": "nothing here\n",
            }),
            cx,
        )
        .await;
        search(&panel, "needle", &mut cx);
        assert_eq!(
            results(&panel, &mut cx),
            vec![("a.rs".to_string(), 1), ("b.rs".to_string(), 2)]
        );
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.visible_rows.len(), 5);
            assert_eq!(panel.status_text().0.as_ref(), "3 results in 2 files");
        });
    }

    #[gpui::test]
    async fn test_collapse_and_navigation(cx: &mut TestAppContext) {
        let (panel, _, _, mut cx) = build_panel(
            json!({
                "a.rs": "needle\nneedle\n",
                "b.rs": "needle\n",
            }),
            cx,
        )
        .await;
        search(&panel, "needle", &mut cx);
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.select_next(&menu::SelectNext, window, cx);
            assert_eq!(panel.selected_row().map(|(index, _)| index), Some(0));
            panel.collapse_selected_entry(&CollapseSelectedEntry, window, cx);
            assert_eq!(panel.visible_rows.len(), 3);
            panel.select_next(&menu::SelectNext, window, cx);
            assert_eq!(
                panel.selected_row().map(|(_, row)| row),
                Some(Row::File { file_index: 1 })
            );
            panel.toggle_collapse_all(&ToggleCollapseAll, window, cx);
            assert_eq!(panel.visible_rows.len(), 2);
            panel.toggle_collapse_all(&ToggleCollapseAll, window, cx);
            assert_eq!(panel.visible_rows.len(), 5);
            panel.dismiss_row(
                Row::Match {
                    file_index: 0,
                    match_index: 0,
                },
                cx,
            );
            assert_eq!(panel.match_count(), 2);
        });
    }

    #[gpui::test]
    async fn test_filters_and_invalid_regex(cx: &mut TestAppContext) {
        let (panel, _, _, mut cx) = build_panel(
            json!({
                "src": { "main.rs": "needle\n" },
                "docs": { "readme.md": "needle\n" },
            }),
            cx,
        )
        .await;
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.filters_enabled = true;
            panel
                .include_editor
                .update(cx, |editor, cx| editor.set_text("src/**", window, cx));
        });
        search(&panel, "needle", &mut cx);
        assert_eq!(
            results(&panel, &mut cx),
            vec![("src/main.rs".to_string(), 1)]
        );

        panel.update(&mut cx, |panel, cx| {
            panel.search_options.insert(SearchOptions::REGEX);
            cx.notify();
        });
        search(&panel, "(unclosed", &mut cx);
        panel.read_with(&cx, |panel, _| {
            assert!(panel.query_error.is_some());
            assert!(panel.file_results.is_empty());
        });
    }

    #[gpui::test]
    async fn test_replace(cx: &mut TestAppContext) {
        let (panel, _, fs, mut cx) = build_panel(
            json!({
                "a.rs": "let foo_1 = foo_2;\n",
                "b.rs": "foo_3\n",
            }),
            cx,
        )
        .await;
        panel.update_in(&mut cx, |panel, window, cx| {
            panel.search_options = SearchOptions::REGEX;
            panel.replace_enabled = true;
            panel
                .replacement_editor
                .update(cx, |editor, cx| editor.set_text("bar_$1", window, cx));
        });
        search(&panel, r"foo_(\d)", &mut cx);
        assert_eq!(
            results(&panel, &mut cx),
            vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1)]
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.replace_row(
                Row::Match {
                    file_index: 0,
                    match_index: 1,
                },
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            fs.load(path!("/dir/a.rs").as_ref()).await.unwrap(),
            "let foo_1 = bar_2;\n"
        );
        assert_eq!(
            results(&panel, &mut cx),
            vec![("a.rs".to_string(), 1), ("b.rs".to_string(), 1)]
        );

        panel.update_in(&mut cx, |panel, window, cx| {
            panel.replace_all(&ReplaceAll, window, cx);
        });
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();
        assert_eq!(
            fs.load(path!("/dir/a.rs").as_ref()).await.unwrap(),
            "let bar_1 = bar_2;\n"
        );
        assert_eq!(
            fs.load(path!("/dir/b.rs").as_ref()).await.unwrap(),
            "bar_3\n"
        );
        assert!(results(&panel, &mut cx).is_empty());
    }
}
