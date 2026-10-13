use super::*;

/// How many folders the workspace menu offers before "Choose folder…".
const KNOWN_WORKSPACES: usize = 8;

impl TacetaApp {
    /// Whether the open conversation works in a folder (Code) or only talks (Chat).
    pub(super) fn code_mode(&self) -> bool {
        self.state.active_conversation().is_code()
    }

    /// The conversation whose request is running, if one is.
    pub(super) fn busy_conversation(&self) -> Option<Uuid> {
        self.generation
            .as_ref()
            .map(|generation| generation.conversation_id)
            .or_else(|| self.agent_ui.running())
            .or_else(|| self.gpt_ui.active.as_ref().map(gpt_ui::ActiveGpt::conversation))
            .or_else(|| self.cli_ui.active.as_ref().map(cli_ui::ActiveCli::conversation))
    }

    /// A conversation that has begun keeps its folder in the title bar; one
    /// that has not offers it above the composer.
    pub(super) fn session_started(&self) -> bool {
        self.state.active_conversation().has_history()
            || self.agent_ui.has_record(self.state.active_conversation_id)
    }

    pub(super) fn set_code_mode(&mut self, code: bool) {
        if self.is_generating() || self.code_mode() == code {
            return;
        }
        let id = self.state.active_conversation_id;
        match self.state.inference_provider {
            InferenceProvider::Gpt => {
                let workspace = code.then(|| self.state.active_conversation().workspace.clone()).flatten();
                self.state.configure_gpt_environment(code, workspace);
            }
            InferenceProvider::Grok | InferenceProvider::Claude => {
                let conversation = self.state.active_conversation_mut();
                conversation.cli_chat = !code;
                // A CLI session belongs to the folder and tools it was started with.
                conversation.cli_sessions.clear();
            }
            // A task's record owns its conversation's transcript, so the
            // other mode starts in a conversation of its own.
            InferenceProvider::Ollama if self.agent_ui.has_record(id) => self.start_conversation(code, None),
            InferenceProvider::Ollama => self.state.active_conversation_mut().agent_enabled = code,
        }
    }

    /// Opens a new conversation in the given mode, in `workspace` when it is
    /// a Code conversation.
    pub(super) fn start_conversation(&mut self, code: bool, workspace: Option<PathBuf>) {
        if self.is_generating() {
            return;
        }
        // A conversation nothing was sent or typed in is already the new one.
        let current = self.state.active_conversation();
        let unused = !current.has_history()
            && current.is_untitled()
            && !self.agent_ui.has_record(current.id)
            && self.state.draft.is_empty()
            && self.state.pending_attachments.is_empty();
        if !unused {
            self.state.start_new_conversation();
        }
        self.screen = Screen::Chat;
        let workspace = workspace.filter(|_| code);
        match self.state.inference_provider {
            InferenceProvider::Gpt => self.state.configure_gpt_environment(code, workspace),
            InferenceProvider::Grok | InferenceProvider::Claude => {
                let conversation = self.state.active_conversation_mut();
                conversation.cli_chat = !code;
                conversation.workspace = workspace;
            }
            InferenceProvider::Ollama => {
                let conversation = self.state.active_conversation_mut();
                conversation.agent_enabled = code;
                conversation.workspace = workspace;
            }
        }
    }

    fn can_change_workspace(&self) -> bool {
        !self.is_generating()
            && !(self.state.inference_provider == InferenceProvider::Ollama
                && self.agent_ui.has_record(self.state.active_conversation_id))
    }

    fn set_workspace(&mut self, path: PathBuf) {
        if !self.can_change_workspace() || self.state.active_conversation().workspace.as_ref() == Some(&path) {
            return;
        }
        match self.state.inference_provider {
            InferenceProvider::Gpt => self.state.configure_gpt_environment(true, Some(path)),
            InferenceProvider::Grok | InferenceProvider::Claude => {
                let conversation = self.state.active_conversation_mut();
                // A CLI session belongs to the folder it was started in.
                conversation.cli_sessions.clear();
                conversation.workspace = Some(path);
            }
            InferenceProvider::Ollama => self.state.active_conversation_mut().workspace = Some(path),
        }
    }

    /// The folders conversations already work in, newest conversation first.
    fn known_workspaces(&self) -> Vec<PathBuf> {
        let mut known: Vec<PathBuf> = Vec::new();
        for conversation in &self.state.conversations {
            if let Some(workspace) = &conversation.workspace {
                if !known.contains(workspace) {
                    known.push(workspace.clone());
                }
            }
        }
        known.truncate(KNOWN_WORKSPACES);
        known
    }

    /// The folder a Code conversation works in, with a menu to choose it,
    /// followed by the branch checked out there.
    pub(super) fn show_workspace_chip(&mut self, ui: &mut Ui) {
        let language = self.language();
        let control = app_shell_control_metrics(ui.ctx());
        let workspace = self.state.active_conversation().workspace.clone();
        let label = workspace
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| text(language, "フォルダーを選ぶ", "Choose folder").into());
        let mut chosen = None;
        let mut browse = false;
        let known = self.known_workspaces();
        ui.add_enabled_ui(self.can_change_workspace(), |ui| {
            let (response, _) = egui::containers::menu::MenuButton::from_button(
                Button::new(label)
                    .min_size(Vec2::new(0.0, control.row_height))
                    .right_text("▼")
                    .corner_radius(control.row_height / 2.0),
            )
            .ui(ui, |ui| {
                ui.spacing_mut().button_padding.y = control.vertical_padding;
                let mut selected = workspace.clone();
                for path in &known {
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    if show_app_shell_menu_row(ui, &mut selected, Some(path.clone()), name)
                        .on_hover_text(path.display().to_string())
                        .clicked()
                    {
                        chosen = Some(path.clone());
                    }
                }
                if !known.is_empty() {
                    ui.separator();
                }
                browse = ui.button(text(language, "フォルダーを選ぶ…", "Choose folder…")).clicked();
            });
            let scope = match self.state.inference_provider {
                InferenceProvider::Ollama => text(language,
                    "このフォルダーを読み取り、編集とコマンドは内容を確認してから実行します。コマンドの通信は無効です。",
                    "Reads this folder. Edits and commands require approval of their exact contents. Commands have no network access."),
                InferenceProvider::Gpt => text(language,
                    "Codexのworkspace-writeで書き込みを制限し、コマンドの通信を無効にします。フォルダーを変えると新しいスレッドを始めます。",
                    "Codex workspace-write limits writes and disables command network access. Changing the folder starts a new thread."),
                InferenceProvider::Grok | InferenceProvider::Claude => text(language,
                    "CLIはこのフォルダーで動きます。フォルダーを変えるとCLIのセッションを新しく始めます。",
                    "The CLI runs in this folder. Changing the folder starts a new CLI session."),
            };
            let path = workspace.as_ref().map(|path| format!("{}\n", path.display())).unwrap_or_default();
            response
                .on_hover_text(format!("{path}{scope}"))
                .on_disabled_hover_text(format!("{path}{}", text(language,
                    "フォルダーの変更は新しい会話で行えます。",
                    "Use a new conversation to change the folder.")));
        });
        if browse {
            chosen = rfd::FileDialog::new().pick_folder();
        }
        if let Some(path) = chosen {
            self.set_workspace(path);
        }
        if let Some(branch) = workspace.as_deref().and_then(|path| cached_branch(ui, path)) {
            ui.label(RichText::new(branch).small().weak());
        }
    }

    /// Which engine answers. It can change at any point of a conversation.
    pub(super) fn show_provider_chip(&mut self, ui: &mut Ui) {
        let control = app_shell_control_metrics(ui.ctx());
        let mut selected = self.state.inference_provider;
        ui.add_enabled_ui(self.can_change_provider(), |ui| {
            egui::containers::menu::MenuButton::from_button(
                Button::new(selected.label())
                    .min_size(Vec2::new(0.0, control.row_height))
                    .right_text("▼")
                    .corner_radius(control.row_height / 2.0),
            )
            .ui(ui, |ui| {
                ui.spacing_mut().button_padding.y = control.vertical_padding;
                for provider in [InferenceProvider::Ollama, InferenceProvider::Grok, InferenceProvider::Gpt, InferenceProvider::Claude] {
                    show_app_shell_menu_row(ui, &mut selected, provider, provider.label());
                }
            });
        });
        self.select_provider(selected);
    }

    /// Whether the line above the composer has anything to say: progress,
    /// a run that can be resumed, or a sign-in that is missing.
    pub(super) fn session_status_visible(&self) -> bool {
        match self.state.inference_provider {
            InferenceProvider::Gpt => self.gpt_status_visible(),
            InferenceProvider::Grok | InferenceProvider::Claude => self.cli_status_visible(),
            InferenceProvider::Ollama => self.agent_status_visible(),
        }
    }

    pub(super) fn show_session_status(&mut self, ui: &mut Ui) {
        match self.state.inference_provider {
            InferenceProvider::Gpt => self.show_gpt_status(ui),
            InferenceProvider::Grok | InferenceProvider::Claude => self.show_cli_status(ui),
            InferenceProvider::Ollama => self.show_agent_status(ui),
        }
    }

    /// The actions and facts that are not needed for every request.
    pub(super) fn show_session_menu(&mut self, ui: &mut Ui) {
        let language = self.language();
        let control = app_shell_control_metrics(ui.ctx());
        let (response, _) = egui::containers::menu::MenuButton::from_button(
            Button::new("…")
                .min_size(Vec2::splat(control.row_height))
                .corner_radius(control.row_height / 2.0),
        )
        .ui(ui, |ui| {
            ui.spacing_mut().button_padding.y = control.vertical_padding;
            ui.checkbox(&mut self.state.show_thinking_trace, text(language, "思考の経過を表示", "Show the thinking trace"))
                .on_hover_text(text(language,
                    "推論を止めず、思考過程の表示だけを切り替えます",
                    "Show or hide the trace without stopping inference"));
            ui.separator();
            match self.state.inference_provider {
                InferenceProvider::Gpt => self.show_gpt_menu_items(ui),
                InferenceProvider::Grok | InferenceProvider::Claude => self.show_cli_menu_items(ui),
                InferenceProvider::Ollama => {
                    self.show_agent_menu_items(ui);
                    let label = if self.model_unload_result.is_some() {
                        text(language, "解放中…", "Releasing…")
                    } else {
                        text(language, "全モデルを解放", "Unload all models")
                    };
                    if ui
                        .add_enabled(self.can_unload_model(), Button::new(label))
                        .on_hover_text(text(language,
                            "接続先で読み込み中の全モデルをメモリから解放します。モデルファイルと会話は残ります。他のアプリが使用するモデルも対象です。",
                            "Unload every model currently loaded on the connected server, including models used by other apps. Files and conversations remain."))
                        .clicked()
                    {
                        self.start_model_unload();
                    }
                }
            }
        });
        response.on_hover_text(text(language, "その他の操作", "More actions"));
    }

    /// Chat or Code for the open conversation, at the top of the sidebar.
    pub(super) fn show_mode_switch(&mut self, ui: &mut Ui) {
        let code = self.code_mode();
        let mut selected = code;
        let width = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        ui.add_enabled_ui(!self.is_generating(), |ui| {
            ui.horizontal(|ui| {
                let palette = theme::palette(ui);
                ui.visuals_mut().selection.bg_fill = palette.selected;
                ui.visuals_mut().selection.stroke.color = palette.contrast_text;
                ui.visuals_mut().widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                for (value, label) in [(false, "Chat"), (true, "Code")] {
                    if ui
                        .add_sized(Vec2::new(width, SIDEBAR_ROW_HEIGHT - 4.0), Button::selectable(selected == value, label))
                        .clicked()
                    {
                        selected = value;
                    }
                }
            });
        });
        if selected != code {
            self.set_code_mode(selected);
            self.screen = Screen::Chat;
        }
    }
}

/// The branch checked out in `workspace`, re-read at most every two seconds.
fn cached_branch(ui: &Ui, workspace: &Path) -> Option<String> {
    let id = egui::Id::new("taceta-workspace-branch");
    let now = ui.input(|input| input.time);
    let cached: Option<(PathBuf, f64, Option<String>)> = ui.data(|data| data.get_temp(id));
    if let Some((path, read_at, branch)) = cached {
        if path == workspace && now - read_at < 2.0 {
            return branch;
        }
    }
    let branch = git_branch(workspace);
    ui.data_mut(|data| data.insert_temp(id, (workspace.to_path_buf(), now, branch.clone())));
    branch
}

/// The branch name from the repository `folder` is in, read from its `HEAD`
/// file. `None` outside a repository; a short commit id when detached.
fn git_branch(folder: &Path) -> Option<String> {
    let git = folder.ancestors().map(|path| path.join(".git")).find(|git| git.exists())?;
    let directory = if git.is_dir() {
        git
    } else {
        // A linked worktree's `.git` is a file that names its directory.
        let pointer = fs::read_to_string(&git).ok()?;
        let target = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
        if target.is_absolute() { target } else { git.parent()?.join(target) }
    };
    let head = fs::read_to_string(directory.join("HEAD")).ok()?;
    let head = head.trim();
    Some(match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => branch.to_owned(),
        None => head.chars().take(7).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_manager_tests::{RecordingServices, test_app};

    #[test]
    fn the_mode_follows_each_engine_and_a_new_conversation_keeps_mode_and_folder() {
        let mut app = test_app(Arc::new(RecordingServices::default()));
        assert!(!app.code_mode());
        app.set_code_mode(true);
        assert!(app.state.active_conversation().agent_enabled);

        let folder = PathBuf::from("/fixture/workspace");
        app.start_conversation(true, Some(folder.clone()));
        // Nothing was sent in the first conversation, so it became the new one.
        assert_eq!(app.state.conversations.len(), 1);
        assert!(app.code_mode() && !app.session_started());
        assert_eq!(app.state.active_conversation().workspace.as_ref(), Some(&folder));
        // A Chat conversation has no folder, whatever the previous one had.
        app.start_conversation(false, Some(folder.clone()));
        assert!(!app.code_mode());
        assert_eq!(app.state.active_conversation().workspace, None);

        // The coding CLIs keep the choice in a field of their own.
        app.state.switch_provider(InferenceProvider::Claude);
        assert!(app.code_mode());
        app.set_code_mode(false);
        assert!(app.state.active_conversation().cli_chat && !app.code_mode());
        app.start_conversation(true, Some(folder.clone()));
        assert!(app.code_mode());
        assert_eq!(app.known_workspaces(), vec![folder]);
    }

    #[test]
    fn the_branch_is_read_from_a_repository_a_subfolder_and_a_linked_worktree() {
        let base = std::env::temp_dir().join(format!("taceta-branch-test-{}", Uuid::new_v4()));
        let repository = base.join("repository");
        fs::create_dir_all(repository.join(".git/worktrees/linked")).unwrap();
        fs::create_dir_all(repository.join("src")).unwrap();
        fs::write(repository.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(git_branch(&repository).as_deref(), Some("main"));
        assert_eq!(git_branch(&repository.join("src")).as_deref(), Some("main"));

        fs::write(repository.join(".git/worktrees/linked/HEAD"), "0123456789abcdef\n").unwrap();
        let linked = base.join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", repository.join(".git/worktrees/linked").display())).unwrap();
        assert_eq!(git_branch(&linked).as_deref(), Some("0123456"));

        let plain = base.join("plain");
        fs::create_dir_all(&plain).unwrap();
        // The temporary directory itself may sit inside a repository.
        if !base.ancestors().any(|path| path.join(".git").exists()) {
            assert_eq!(git_branch(&plain), None);
        }
        fs::remove_dir_all(&base).unwrap();
    }
}
