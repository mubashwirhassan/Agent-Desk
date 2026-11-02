import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

type CliStatus = {
  available: boolean;
  path: string | null;
  version: string | null;
  message: string;
};
type TurnEvent = { turn_id: string; kind: string; text: string | null; session_id: string | null };
type ChatMessage = { id: string; role: "user" | "assistant"; content: string; turnId?: string };
type ProjectEntry = { name: string; path: string; is_dir: boolean; children: ProjectEntry[] };
type Provider = { id: string; name: string; base_url: string; models: string[]; default_model: string; has_api_key: boolean };
type ProviderDraft = { id: string; name: string; base_url: string; api_key: string; models_text: string; default_model: string };
type SavedChat = { id: string; title: string; project_path: string; session_id: string | null; provider_id: string | null; model: string | null; messages: ChatMessage[]; updated_at: number };
type FilePreview = { path: string; content: string; truncated: boolean };
type AuthStatus = { logged_in: boolean; auth_method: string | null; message: string };
type GitStatus = { available: boolean; branch: string | null; files: { path: string; index_status: string; worktree_status: string }[]; message: string };
type PermissionRequest = { permission_id: string; turn_id: string; tool_name: string; input: unknown };
type FileMention = { start: number; end: number; query: string };

function readSavedChats(): SavedChat[] {
  try {
    const saved = JSON.parse(localStorage.getItem("agentdeck.chats.v1") || "[]");
    return Array.isArray(saved) ? saved as SavedChat[] : [];
  } catch { return []; }
}

function flattenProjectFiles(entries: ProjectEntry[]): ProjectEntry[] {
  return entries.flatMap((entry) => entry.is_dir ? flattenProjectFiles(entry.children) : [entry]);
}

export default function App() {
  const [status, setStatus] = useState<CliStatus | null>(null);
  const [authStatus, setAuthStatus] = useState<AuthStatus | null>(null);
  const [authLoading, setAuthLoading] = useState(false);
  const [setupOpen, setSetupOpen] = useState(() => localStorage.getItem("agentdeck.setup.complete") !== "true");
  const [theme, setTheme] = useState(() => localStorage.getItem("agentdeck.theme") || "dark");
  const [permissionRequest, setPermissionRequest] = useState<PermissionRequest | null>(null);
  const [gitStatus, setGitStatus] = useState<GitStatus | null>(null);
  const [gitPopoverOpen, setGitPopoverOpen] = useState(false);
  const [projectPath, setProjectPath] = useState<string | null>(null);
  const [project, setProject] = useState("Choose a project folder");
  const [projectFiles, setProjectFiles] = useState<ProjectEntry[]>([]);
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(new Set());
  const [fileQuery, setFileQuery] = useState("");
  const [filePreview, setFilePreview] = useState<FilePreview | null>(null);
  const [filePreviewError, setFilePreviewError] = useState("");
  const [chatQuery, setChatQuery] = useState("");
  const [providers, setProviders] = useState<Provider[]>([]);
  const [selectedProviderId, setSelectedProviderId] = useState("");
  const [selectedModel, setSelectedModel] = useState("");
  const [modelOverride, setModelOverride] = useState("");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [providerDraft, setProviderDraft] = useState<ProviderDraft | null>(null);
  const [settingsError, setSettingsError] = useState("");
  const [providerFeedback, setProviderFeedback] = useState("");
  const [providerTesting, setProviderTesting] = useState(false);
  const [modelsDiscovering, setModelsDiscovering] = useState(false);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [savedChats, setSavedChats] = useState<SavedChat[]>(readSavedChats);
  const [conversationId, setConversationId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [fileMention, setFileMention] = useState<FileMention | null>(null);
  const [mentionIndex, setMentionIndex] = useState(0);
  const [busy, setBusy] = useState(false);
  const [activeTurn, setActiveTurn] = useState<string | null>(null);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const activeTurnRef = useRef<string | null>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const projectPathRef = useRef<string | null>(null);
  const turnListenerRef = useRef<(() => void) | null>(null);
  const permissionListenerRef = useRef<(() => void) | null>(null);
  projectPathRef.current = projectPath;

  const refreshStatus = () => invoke<CliStatus>("detect_claude").then(setStatus).catch(() => {
    setStatus({ available: false, path: null, version: null, message: "Could not check Claude Code status." });
  });

  const refreshAuthStatus = async () => {
    if (!status?.available) { setAuthStatus(null); return; }
    setAuthLoading(true);
    try { setAuthStatus(await invoke<AuthStatus>("check_auth_status")); }
    catch (error) { setAuthStatus({ logged_in: false, auth_method: null, message: typeof error === "string" ? error : "Could not check Claude Code login." }); }
    finally { setAuthLoading(false); }
  };

  const refreshGitStatus = async (path: string | null) => {
    if (!path) { setGitStatus(null); return; }
    try { setGitStatus(await invoke<GitStatus>("get_git_status", { projectPath: path })); }
    catch { setGitStatus({ available: false, branch: null, files: [], message: "Could not read Git status." }); }
  };

  const refreshProviders = () => invoke<Provider[]>("get_providers").then(setProviders).catch((error) => {
    setSettingsError(typeof error === "string" ? error : "Could not load provider settings.");
  });

  async function refreshProjectFiles(path: string) {
    try {
      setProjectFiles(await invoke<ProjectEntry[]>("list_project_files", { projectPath: path }));
      setExpandedFolders(new Set([path]));
    } catch {
      setProjectFiles([]);
      setExpandedFolders(new Set());
    }
  }

  useEffect(() => {
    void refreshStatus();
    void refreshProviders();
    void listen<TurnEvent>("claude-turn", ({ payload }) => {
      if (payload.turn_id !== activeTurnRef.current) return;
      if (payload.kind === "text" && payload.text) {
        setMessages((old) => old.map((message) => message.turnId === payload.turn_id
          ? { ...message, content: message.content + payload.text }
          : message));
      } else if (payload.kind === "session" && payload.session_id) {
        setSessionId(payload.session_id);
      } else if (payload.kind === "permission-denied" && payload.text) {
        setMessages((old) => old.map((message) => message.turnId === payload.turn_id
          ? { ...message, content: `${message.content}${message.content ? "\n\n" : ""}${payload.text}` }
          : message));
      } else if (payload.kind === "error" || payload.kind === "complete") {
        setPermissionRequest((current) => current?.turn_id === payload.turn_id ? null : current);
        if (payload.kind === "error" && payload.text) {
          setMessages((old) => old.map((message) => message.turnId === payload.turn_id
            ? { ...message, content: `${message.content}${message.content ? "\n\n" : ""}${payload.text}` }
            : message));
        }
        setBusy(false);
        activeTurnRef.current = null;
        setActiveTurn(null);
        void refreshGitStatus(projectPathRef.current);
        inputRef.current?.focus();
      }
    }).then((stop) => { turnListenerRef.current = stop; });
    void listen<PermissionRequest>("permission-request", ({ payload }) => setPermissionRequest(payload))
      .then((stop) => { permissionListenerRef.current = stop; });
    return () => { turnListenerRef.current?.(); permissionListenerRef.current?.(); };
  }, []);

  useEffect(() => {
    if (status?.available) void refreshAuthStatus();
    else setAuthStatus(null);
  }, [status?.available]);

  useEffect(() => { localStorage.setItem("agentdeck.theme", theme); }, [theme]);

  useEffect(() => {
    if (!conversationId || !projectPath || messages.length === 0) return;
    const firstPrompt = messages.find((message) => message.role === "user")?.content || "New chat";
    const chat: SavedChat = {
      id: conversationId,
      title: firstPrompt.replace(/\s+/g, " ").slice(0, 56),
      project_path: projectPath,
      session_id: sessionId,
      provider_id: selectedProviderId || null,
      model: selectedProviderId ? (selectedModel || providers.find((provider) => provider.id === selectedProviderId)?.default_model || null) : (modelOverride.trim() || null),
      messages,
      updated_at: Date.now(),
    };
    setSavedChats((old) => [chat, ...old.filter((item) => item.id !== chat.id)].slice(0, 40));
  }, [conversationId, projectPath, sessionId, selectedProviderId, selectedModel, modelOverride, providers, messages]);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      try { localStorage.setItem("agentdeck.chats.v1", JSON.stringify(savedChats)); } catch { /* keep current chat usable if browser storage is full */ }
    }, 500);
    return () => window.clearTimeout(timer);
  }, [savedChats]);

  async function chooseProject() {
    const selected = await open({ directory: true, multiple: false, title: "Choose a project folder" });
    if (typeof selected === "string") {
      setProjectPath(selected);
      const segments = selected.split(/[\\/]/).filter(Boolean);
      setProject(segments[segments.length - 1] || selected);
      void refreshProjectFiles(selected);
      void refreshGitStatus(selected);
      setConversationId(null);
      setSessionId(null);
      setMessages([]);
    }
  }

  async function beginLogin() {
    try { await invoke("open_auth_login"); }
    catch (error) { setAuthStatus({ logged_in: false, auth_method: null, message: typeof error === "string" ? error : "Could not open a terminal for login." }); }
  }

  function finishSetup() {
    localStorage.setItem("agentdeck.setup.complete", "true");
    setSetupOpen(false);
  }

  async function answerPermission(allow: boolean) {
    if (!permissionRequest) return;
    const current = permissionRequest;
    setPermissionRequest(null);
    try { await invoke("answer_permission", { permissionId: current.permission_id, allow }); }
    catch (error) {
      setMessages((old) => [...old, { id: crypto.randomUUID(), role: "assistant", content: typeof error === "string" ? error : "Could not deliver the permission decision." }]);
    }
  }

  function updateDraft(value: string, cursor: number) {
    setDraft(value);
    const beforeCaret = value.slice(0, cursor);
    const match = beforeCaret.match(/(?:^|\s)@([^\s@]*)$/);
    if (!match) { setFileMention(null); return; }
    const query = match[1];
    setFileMention({ start: cursor - query.length - 1, end: cursor, query });
    setMentionIndex(0);
  }

  function insertMention(entry: ProjectEntry) {
    if (!fileMention || !projectPath) return;
    const relativePath = entry.path.slice(projectPath.length).replace(/^[\\/]+/, "").replace(/\\/g, "/");
    const insertion = `@${relativePath} `;
    const next = `${draft.slice(0, fileMention.start)}${insertion}${draft.slice(fileMention.end)}`;
    setDraft(next);
    setFileMention(null);
    requestAnimationFrame(() => {
      const input = inputRef.current;
      if (!input) return;
      const cursor = fileMention.start + insertion.length;
      input.focus();
      input.setSelectionRange(cursor, cursor);
    });
  }

  async function sendMessage() {
    const prompt = draft.trim();
    if (!prompt || !projectPath || busy || !status?.available) return;
    const turnId = crypto.randomUUID();
    if (!conversationId) setConversationId(crypto.randomUUID());
    activeTurnRef.current = turnId;
    setActiveTurn(turnId);
    setBusy(true);
    setDraft("");
    setMessages((old) => [...old,
      { id: crypto.randomUUID(), role: "user", content: prompt },
      { id: crypto.randomUUID(), role: "assistant", content: "", turnId },
    ]);
    try {
      const provider = providers.find((item) => item.id === selectedProviderId);
      const model = provider ? (selectedModel || provider.default_model) : modelOverride.trim();
      await invoke("start_turn", { turnId, prompt, projectPath, sessionId, providerId: provider?.id ?? null, model: model || null });
    } catch (error) {
      const message = typeof error === "string" ? error : "Could not start Claude Code.";
      setMessages((old) => old.map((item) => item.turnId === turnId ? { ...item, content: message } : item));
      setBusy(false);
      activeTurnRef.current = null;
      setActiveTurn(null);
    }
  }

  async function stopTurn() {
    if (!activeTurn) return;
    try { await invoke("stop_turn", { turnId: activeTurn }); } catch { /* final process event reports its state */ }
  }

  function newChat() {
    if (busy) return;
    setMessages([]);
    setSessionId(null);
    setConversationId(null);
    setDraft("");
    inputRef.current?.focus();
  }

  function openSavedChat(chat: SavedChat) {
    if (busy) return;
    setConversationId(chat.id);
    setProjectPath(chat.project_path);
    const segments = chat.project_path.split(/[\\/]/).filter(Boolean);
    setProject(segments[segments.length - 1] || chat.project_path);
    void refreshProjectFiles(chat.project_path);
    void refreshGitStatus(chat.project_path);
    setSessionId(chat.session_id);
    setSelectedProviderId(chat.provider_id || "");
    setSelectedModel(chat.model || "");
    setModelOverride(chat.provider_id ? "" : chat.model || "");
    setMessages(chat.messages);
    setDraft("");
  }

  function beginNewProvider() {
    setProviderDraft({ id: crypto.randomUUID(), name: "", base_url: "", api_key: "", models_text: "", default_model: "" });
    setSettingsError("");
    setProviderFeedback("");
  }

  function editProvider(provider: Provider) {
    setProviderDraft({ id: provider.id, name: provider.name, base_url: provider.base_url, api_key: "", models_text: provider.models.join("\n"), default_model: provider.default_model });
    setSettingsError("");
    setProviderFeedback("");
  }

  async function saveProvider() {
    if (!providerDraft) return;
    setSettingsError("");
    setProviderFeedback("");
    try {
      const updated = await invoke<Provider[]>("save_provider", { input: providerInputFromDraft(providerDraft) });
      setProviders(updated);
      setSelectedProviderId(providerDraft.id);
      setSelectedModel(providerDraft.default_model);
      setModelOverride("");
      setProviderDraft(null);
      setProviderFeedback("");
    } catch (error) {
      setSettingsError(typeof error === "string" ? error : "Could not save provider.");
    }
  }

  async function removeProvider(provider: Provider) {
    setSettingsError("");
    try {
      setProviders(await invoke<Provider[]>("delete_provider", { providerId: provider.id }));
      if (selectedProviderId === provider.id) {
        setSelectedProviderId("");
        setSelectedModel("");
      }
      setProviderDraft(null);
    } catch (error) {
      setSettingsError(typeof error === "string" ? error : "Could not remove provider.");
    }
  }

  function providerInputFromDraft(draft: ProviderDraft) {
    return {
      id: draft.id,
      name: draft.name,
      baseUrl: draft.base_url,
      apiKey: draft.api_key,
      models: draft.models_text.split("\n").map((value) => value.trim()).filter(Boolean),
      defaultModel: draft.default_model,
    };
  }

  async function testProvider() {
    if (!providerDraft) return;
    setProviderTesting(true);
    setProviderFeedback("");
    setSettingsError("");
    try {
      setProviderFeedback(await invoke<string>("test_provider", { input: providerInputFromDraft(providerDraft) }));
    } catch (error) {
      setSettingsError(typeof error === "string" ? error : "Provider test failed.");
    } finally { setProviderTesting(false); }
  }

  async function discoverModels() {
    if (!providerDraft) return;
    setModelsDiscovering(true);
    setProviderFeedback("");
    setSettingsError("");
    try {
      const found = await invoke<string[]>("discover_provider_models", { input: providerInputFromDraft(providerDraft) });
      setProviderDraft((draft) => {
        if (!draft) return draft;
        const combined = [...new Set([...draft.models_text.split("\n").map((item) => item.trim()).filter(Boolean), ...found])];
        return { ...draft, models_text: combined.join("\n"), default_model: draft.default_model || found[0] || "" };
      });
      setProviderFeedback(`Found ${found.length} model${found.length === 1 ? "" : "s"}. Save to add them.`);
    } catch (error) {
      setSettingsError(typeof error === "string" ? error : "Model discovery failed.");
    } finally { setModelsDiscovering(false); }
  }

  const activeProvider = providers.find((provider) => provider.id === selectedProviderId);

  async function openFilePreview(path: string) {
    if (!projectPath) return;
    setFilePreviewError("");
    try { setFilePreview(await invoke<FilePreview>("read_project_file", { projectPath, filePath: path })); }
    catch (error) { setFilePreviewError(typeof error === "string" ? error : "Could not preview this file."); }
  }

  function toggleFolder(path: string) {
    setExpandedFolders((old) => {
      const next = new Set(old);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  function renderFiles(entries: ProjectEntry[], level = 0): JSX.Element[] {
    const query = fileQuery.trim().toLowerCase();
    const filtered = query ? entries.flatMap((entry) => {
      const children = entry.is_dir ? filteredChildren(entry.children, query) : [];
      return entry.name.toLowerCase().includes(query) || children.length ? [{ ...entry, children }] : [];
    }) : entries;
    return filtered.map((entry) => {
      const expanded = expandedFolders.has(entry.path) || Boolean(query);
      return <div key={entry.path}>
        <button className={`file-entry ${entry.is_dir ? "directory" : "file"}`} style={{ paddingLeft: `${9 + level * 13}px` }} onClick={() => entry.is_dir ? toggleFolder(entry.path) : void openFilePreview(entry.path)} title={entry.path}>
          <span className="file-entry-icon">{entry.is_dir ? (expanded ? "▾" : "▸") : "·"}</span><span>{entry.name}</span>
        </button>
        {entry.is_dir && expanded && entry.children.length > 0 && renderFiles(entry.children, level + 1)}
      </div>;
    });
  }

  function filteredChildren(entries: ProjectEntry[], query: string): ProjectEntry[] {
    return entries.flatMap((entry) => {
      const children = entry.is_dir ? filteredChildren(entry.children, query) : [];
      return entry.name.toLowerCase().includes(query) || children.length ? [{ ...entry, children }] : [];
    });
  }

  const visibleChats = savedChats.filter((chat) => chat.title.toLowerCase().includes(chatQuery.trim().toLowerCase())).slice(0, 12);
  const visibleFiles = projectPath ? renderFiles(projectFiles) : [];
  const mentionCandidates = fileMention && projectPath ? flattenProjectFiles(projectFiles).filter((entry) => {
    const relativePath = entry.path.slice(projectPath.length).replace(/^[\\/]+/, "").replace(/\\/g, "/");
    return !fileMention.query || relativePath.toLowerCase().includes(fileMention.query.toLowerCase());
  }).slice(0, 8) : [];

  return (
    <div className="app-shell" data-theme={theme}>
      <aside className="sidebar">
        <div className="brand"><span className="brand-mark">A</span><span>AgentDeck</span></div>
        <button className="new-chat" onClick={newChat}><span>＋</span> New chat</button>
        <label className="chat-search"><span>⌕</span><input value={chatQuery} onChange={(event) => setChatQuery(event.target.value)} placeholder="Search chats" aria-label="Search chat history" /></label>
        <div className="side-label">RECENT</div>
        {visibleChats.length ? visibleChats.map((chat) => <button key={chat.id} className={`recent-chat ${conversationId === chat.id ? "active" : ""}`} onClick={() => openSavedChat(chat)} title={chat.title}>◉ &nbsp;{chat.title}</button>) : <div className="empty-recent">{chatQuery ? "No matching chats." : "Your conversations will appear here."}</div>}
        <div className="side-label files-label">FILES</div>
        {projectPath && <label className="file-search"><span>⌕</span><input value={fileQuery} onChange={(event) => setFileQuery(event.target.value)} placeholder="Search project files" aria-label="Search project files" /></label>}
        <div className="file-tree">
          {!projectPath ? <div className="empty-recent">Choose a project folder to see its files.</div> : visibleFiles.length ? visibleFiles : <div className="empty-recent">{fileQuery ? "No matching files." : "No files to show."}</div>}
          {filePreviewError && <div className="file-preview-error">{filePreviewError}</div>}
        </div>
        <div className="sidebar-bottom">
          <div className="connection-card">
            <span className={`status-dot ${status?.available ? "ready" : status ? "missing" : ""}`} />
            <div><strong>{status?.available ? "Claude Code found" : status ? "Claude Code not found" : "Checking Claude Code…"}</strong><small>{status?.version ?? "Using Claude Code configuration"}</small></div>
            <button className="icon-button" aria-label="Refresh connection status" onClick={refreshStatus}>↻</button>
          </div>
          <button className="settings-button" onClick={() => setSetupOpen(true)}>◉ <span>Setup &amp; diagnostics</span></button>
          <button className="settings-button" onClick={() => { setSettingsOpen(true); void refreshProviders(); }}>⚙ <span>Providers &amp; models</span></button>
        </div>
      </aside>

      <main className="main-panel">
        <header className="topbar">
          <div className="topbar-left"><button className="project-picker" onClick={chooseProject}><span className="folder-icon">▰</span><span>{project}</span><span className="chevron">⌄</span></button>
            {projectPath && <div className="git-status-wrap"><button className={`git-status-button ${gitStatus?.files.length ? "has-changes" : ""}`} onClick={() => setGitPopoverOpen((open) => !open)}><span className="git-branch-icon">⑂</span><span>{gitStatus?.branch || (gitStatus?.available ? "Git" : "No Git")}</span>{gitStatus?.files.length ? <span className="git-change-count">{gitStatus.files.length}</span> : null}</button>
              {gitPopoverOpen && <div className="git-popover"><div className="git-popover-heading"><strong>{gitStatus?.branch || "Git status"}</strong><button className="icon-button" onClick={() => void refreshGitStatus(projectPath)} aria-label="Refresh Git status">↻</button></div>
                {!gitStatus?.available ? <p className="git-empty">{gitStatus?.message || "Checking repository…"}</p> : gitStatus.files.length ? <div className="git-file-list">{gitStatus.files.map((file) => <div key={`${file.path}-${file.index_status}-${file.worktree_status}`}><code>{file.index_status}{file.worktree_status}</code><span>{file.path}</span></div>)}</div> : <p className="git-empty">Working tree clean</p>}
                <small>Read-only status · no Git changes are made by AgentDeck</small></div>}
            </div>}
          </div>
          <button className="avatar" aria-label="Setup and diagnostics" onClick={() => setSetupOpen(true)}>{authStatus?.logged_in ? "✓" : "!"}</button>
        </header>
        <section className={`conversation-area ${messages.length ? "has-messages" : ""}`}>
          {messages.length === 0 ? <div className="welcome">
            <div className="welcome-icon">✳</div>
            <p className="eyebrow">YOUR LOCAL CODING PARTNER</p>
            <h1>What are we building?</h1>
            <p className="welcome-copy">Send a message to your locally installed Claude Code. Its response will appear here.</p>
            {!projectPath && <p className="setup-hint">Choose a project folder to view its files and enable sending.</p>}
            {status && !status.available && <div className="notice"><span>ⓘ</span><div><strong>Claude Code needs setup</strong><p>{status.message}</p></div></div>}
            <div className="suggestions">
              {["Understand this project", "Plan a change", "Find a bug"].map((label, index) => <button key={label} onClick={() => { setDraft(["Give me a guided tour of this codebase.", "Explore this project and propose a plan for a change.", "Investigate a bug in this project."][index]); inputRef.current?.focus(); }}><span>{["⌘", "✎", "⌕"][index]}</span><div><strong>{label}</strong><small>{["Get a guided tour of the codebase", "Explore an implementation approach", "Investigate an issue in your code"][index]}</small></div></button>)}
            </div>
          </div> : <div className="transcript">{messages.map((message) => <article className={`message ${message.role}`} key={message.id}>
            <div className="message-avatar">{message.role === "user" ? "H" : "✳"}</div>
            <div className="message-content">{message.content || (busy && message.role === "assistant" ? <span className="typing">Claude Code is working…</span> : "")}</div>
          </article>)}</div>}
        </section>
        <div className="composer-wrap">
          {fileMention && mentionCandidates.length > 0 && <div className="mention-menu" role="listbox" aria-label="Project files">{mentionCandidates.map((entry, index) => {
            const relativePath = entry.path.slice(projectPath!.length).replace(/^[\\/]+/, "").replace(/\\/g, "/");
            return <button key={entry.path} className={index === mentionIndex ? "active" : ""} role="option" aria-selected={index === mentionIndex} onMouseDown={(event) => event.preventDefault()} onClick={() => insertMention(entry)}><span className="mention-file-icon">▤</span><span>{relativePath}</span></button>;
          })}</div>}
          <div className="composer">
          <textarea ref={inputRef} value={draft} onChange={(event) => updateDraft(event.target.value, event.target.selectionStart)} onKeyDown={(event) => { if (fileMention && mentionCandidates.length && event.key === "ArrowDown") { event.preventDefault(); setMentionIndex((index) => (index + 1) % mentionCandidates.length); } else if (fileMention && mentionCandidates.length && event.key === "ArrowUp") { event.preventDefault(); setMentionIndex((index) => (index - 1 + mentionCandidates.length) % mentionCandidates.length); } else if (fileMention && mentionCandidates.length && event.key === "Enter" && !event.shiftKey) { event.preventDefault(); insertMention(mentionCandidates[mentionIndex] || mentionCandidates[0]); } else if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void sendMessage(); } if (event.key === "Escape" && fileMention) setFileMention(null); else if (event.key === "Escape" && busy) void stopTurn(); }} placeholder={!projectPath ? "Write a message — choose a project before sending…" : status?.available ? "Message Claude Code… (type @ to attach a project file)" : "Write a message — Claude Code must be installed to send…"} rows={2} aria-label="Message Claude Code" />
            <div className="composer-footer">
              <div className="composer-meta model-meta">
                <span className="project-chip">▰ {project}</span>
                <select aria-label="Provider" className="model-select provider-select" value={selectedProviderId} onChange={(event) => {
                  const id = event.target.value;
                  setSelectedProviderId(id);
                  const provider = providers.find((item) => item.id === id);
                  setSelectedModel(provider?.default_model ?? "");
                  setModelOverride("");
                }}>
                  <option value="">Claude Code config</option>
                  {providers.map((provider) => <option key={provider.id} value={provider.id}>{provider.name}</option>)}
                </select>
                {activeProvider ? <select aria-label="Model" className="model-select model-choice" value={selectedModel || activeProvider.default_model} onChange={(event) => setSelectedModel(event.target.value)}>
                  {activeProvider.models.map((model) => <option key={model} value={model}>{model}</option>)}
                </select> : <input aria-label="Model override" className="model-select model-override" value={modelOverride} onChange={(event) => setModelOverride(event.target.value)} placeholder="Default model" />}
                <span className="safe-chip">Ask before file edits or commands</span>
              </div>
              {busy ? <button className="stop-button" onClick={stopTurn} aria-label="Stop response">■</button> : <button className="send-button" onClick={sendMessage} disabled={!draft.trim() || !projectPath || !status?.available} aria-label="Send message">↑</button>}
            </div>
          </div>
          <p className="privacy-note">{activeProvider ? `Using ${activeProvider.name}; its API key is stored in the desktop keyring.` : "AgentDeck runs your local Claude Code CLI with its existing configuration and authentication."}</p>
        </div>
      </main>
      {settingsOpen && <div className="settings-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) setSettingsOpen(false); }}>
        <section className="settings-panel" role="dialog" aria-modal="true" aria-labelledby="settings-title">
          <header className="settings-header"><div><h2 id="settings-title">Providers &amp; models</h2><p>Add an Anthropic-compatible endpoint for Claude Code.</p></div><div className="settings-header-actions"><button className="theme-toggle" onClick={() => setTheme((value) => value === "dark" ? "light" : "dark")}>{theme === "dark" ? "☼ Light" : "◐ Dark"}</button><button className="icon-button close-settings" aria-label="Close settings" onClick={() => setSettingsOpen(false)}>×</button></div></header>
          <div className="settings-notice">Provider keys are stored in the desktop keyring. AgentDeck passes provider URL, key, and model to Claude Code only for that request; it does not edit Claude Code's global settings.</div>
          <div className="provider-manager">
            <div className="provider-list"><button className="add-provider-button" onClick={beginNewProvider}>＋ Add provider</button>
              {providers.map((provider) => <div className="provider-row" key={provider.id}><button className="provider-row-main" onClick={() => editProvider(provider)}><strong>{provider.name}</strong><small>{provider.models.length} models · {provider.has_api_key ? "key saved" : "key missing"}</small></button><button className="icon-button remove-provider" title={`Remove ${provider.name}`} aria-label={`Remove ${provider.name}`} onClick={() => { if (window.confirm(`Remove ${provider.name} and its saved API key?`)) void removeProvider(provider); }}>×</button></div>)}
              {providers.length === 0 && <p className="empty-providers">No custom providers yet.</p>}
            </div>
            <div className="provider-form">
              {providerDraft ? <>
                <div className="provider-form-heading"><h3>{providers.some((provider) => provider.id === providerDraft.id) ? "Edit provider" : "New provider"}</h3><button className="text-button" onClick={beginNewProvider}>New</button></div>
                <label>Provider name<input value={providerDraft.name} onChange={(event) => setProviderDraft({ ...providerDraft, name: event.target.value })} placeholder="My API provider" /></label>
                <label>Anthropic-compatible base URL<input value={providerDraft.base_url} onChange={(event) => setProviderDraft({ ...providerDraft, base_url: event.target.value })} placeholder="https://api.example.com" /></label>
                <label>API key<input type="password" autoComplete="new-password" value={providerDraft.api_key} onChange={(event) => setProviderDraft({ ...providerDraft, api_key: event.target.value })} placeholder={providers.some((provider) => provider.id === providerDraft.id) ? "Leave blank to keep saved key" : "Paste API key"} /></label>
                <label>Model IDs <small>One model ID per line</small><textarea className="models-input" value={providerDraft.models_text} onChange={(event) => setProviderDraft({ ...providerDraft, models_text: event.target.value })} placeholder={"provider-model-1\nprovider-model-2"} rows={4} /><button className="discover-models-button" onClick={discoverModels} disabled={modelsDiscovering}>{modelsDiscovering ? "Finding models…" : "↻ Discover models"}</button></label>
                <label>Default model<input list="provider-model-list" value={providerDraft.default_model} onChange={(event) => setProviderDraft({ ...providerDraft, default_model: event.target.value })} placeholder="Choose or type a model ID" /><datalist id="provider-model-list">{providerDraft.models_text.split("\n").map((model) => model.trim()).filter(Boolean).map((model) => <option key={model} value={model} />)}</datalist></label>
                <p className="provider-cost-note">Test sends a short request to this model; your provider may bill for it.</p>
                {providerFeedback && <p className="settings-success">{providerFeedback}</p>}
                {settingsError && <p className="settings-error">{settingsError}</p>}
                <div className="provider-form-actions"><button className="text-button" onClick={() => setProviderDraft(null)}>Cancel</button><button className="test-provider-button" onClick={testProvider} disabled={providerTesting}>{providerTesting ? "Testing…" : "Test provider"}</button><button className="save-provider-button" onClick={saveProvider}>Save provider</button></div>
              </> : <div className="provider-empty-state"><span>⌘</span><strong>Add a provider to choose its models in the chat bar.</strong><p>Use an Anthropic Messages API compatible endpoint. OpenAI-only endpoints are not compatible with Claude Code.</p>{settingsError && <p className="settings-error">{settingsError}</p>}</div>}
            </div>
          </div>
        </section>
      </div>}
      {filePreview && <div className="file-preview-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) setFilePreview(null); }}>
        <section className="file-preview-panel" role="dialog" aria-modal="true" aria-labelledby="file-preview-title">
          <header className="file-preview-header"><div><h2 id="file-preview-title">{filePreview.path.split(/[\\/]/).pop()}</h2><p>{filePreview.path}</p></div><button className="icon-button close-settings" aria-label="Close file preview" onClick={() => setFilePreview(null)}>×</button></header>
          {filePreview.truncated && <div className="preview-truncated">Preview limited to the first 512 KB.</div>}
          <pre className="file-preview-content">{filePreview.content}</pre>
        </section>
      </div>}
      {setupOpen && <div className="setup-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) finishSetup(); }}>
        <section className="setup-panel" role="dialog" aria-modal="true" aria-labelledby="setup-title">
          <header className="setup-heading"><div className="setup-mark">A</div><div><p className="eyebrow">AGENTDECK SETUP</p><h2 id="setup-title">Get your local workspace ready</h2><p>Claude Code stays installed and signed in separately. AgentDeck connects to that local CLI.</p></div><button className="icon-button" aria-label="Close setup" onClick={finishSetup}>×</button></header>
          <div className="setup-steps">
            <article className={`setup-step ${status?.available ? "complete" : status ? "needs-action" : ""}`}><span className="setup-step-number">{status?.available ? "✓" : "1"}</span><div><strong>Claude Code CLI</strong><p>{status?.available ? `${status.version || "Installed"} · ${status.path}` : status?.message || "Checking for the local CLI…"}</p>{status && !status.available && <button className="inline-action" onClick={refreshStatus}>Check again</button>}</div></article>
            <article className={`setup-step ${authStatus?.logged_in ? "complete" : authStatus ? "needs-action" : ""}`}><span className="setup-step-number">{authStatus?.logged_in ? "✓" : "2"}</span><div><strong>Claude Code login</strong><p>{authLoading ? "Checking login status…" : authStatus?.message || (status?.available ? "Check the login that Claude Code already uses." : "Available after Claude Code is installed.")}</p><div className="setup-actions">{status?.available && !authStatus?.logged_in && <button className="inline-action" onClick={beginLogin}>Sign in through Claude Code</button>}{status?.available && <button className="inline-action secondary" onClick={refreshAuthStatus}>Refresh login status</button>}</div></div></article>
            <article className={`setup-step ${projectPath ? "complete" : ""}`}><span className="setup-step-number">{projectPath ? "✓" : "3"}</span><div><strong>Choose a project</strong><p>{projectPath || "Select the folder Claude Code should work in."}</p><button className="inline-action" onClick={() => void chooseProject()}>{projectPath ? "Change project folder" : "Choose project folder"}</button></div></article>
          </div>
          <div className="setup-privacy"><span>ⓘ</span><p>Project files and chats stay on this computer. Claude Code connects to its configured provider for model requests. Tool actions that need permission will pause here until you Allow once or Deny.</p></div>
          <footer className="setup-footer"><button className="text-button" onClick={finishSetup}>Finish later</button><button className="save-provider-button" onClick={finishSetup} disabled={!status?.available || !authStatus?.logged_in || !projectPath}>Open AgentDeck</button></footer>
        </section>
      </div>}
      {permissionRequest && <div className="permission-backdrop">
        <section className="permission-panel" role="alertdialog" aria-modal="true" aria-labelledby="permission-title">
          <div className="permission-icon">!</div><p className="eyebrow">CLAUDE CODE NEEDS APPROVAL</p><h2 id="permission-title">Allow this action?</h2>
          <p className="permission-tool">{permissionRequest.tool_name}</p>
          <pre className="permission-details">{JSON.stringify(permissionRequest.input, null, 2)}</pre>
          <p className="permission-note">This decision applies once to this action. AgentDeck does not add a permanent allow rule.</p>
          <div className="permission-actions"><button className="deny-action" onClick={() => void answerPermission(false)}>Deny</button><button className="allow-action" onClick={() => void answerPermission(true)}>Allow once</button></div>
        </section>
      </div>}
    </div>
  );
}
