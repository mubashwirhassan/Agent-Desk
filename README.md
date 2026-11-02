# AgentDeck

AgentDeck is a local desktop chat interface for the user's installed Claude Code CLI. It launches `claude` directly, in the selected project folder, and uses Claude Code's existing configuration and authentication. It does not include Claude Code or an AI engine.

## Current status

The Tauri 2 + React desktop shell and direct Claude Code process runner are implemented. The Debian package has been built in the user's environment. AgentDeck detects the local `claude` executable and keeps sending disabled if it is missing.

## Planned development setup

For development, install Node.js/npm and Rust/Cargo (Tauri's Linux system libraries are separate prerequisites), install Claude Code using its official instructions, ensure `claude` is available on the desktop app's `PATH`, then run `npm install` and `npm run tauri -- build`. The generated Debian package is under `src-tauri/target/release/bundle/deb/`. AgentDeck does not read Claude Code credential files.

To develop the frontend, run `npm run tauri -- dev`. First-run setup checks the local CLI, its non-secret authentication status, and the project folder. Login opens Claude Code's own `claude auth login` command in a terminal. In chat, type `@` to search and attach a project file to the next prompt. Claude Code output is streamed into the conversation and follow-up turns resume the actual session. When an action needs permission, AgentDeck shows the exact tool and input with **Allow once** and **Deny**; it never saves a permanent allow rule. A temporary local MCP permission tool carries that decision back to Claude Code. Git branch and changed-file status are read-only. Chats remain local in WebView storage, and the light/dark theme is saved locally.

Use **Providers & models** to add an Anthropic-compatible endpoint and API key, test it, discover models when the endpoint supports `/v1/models`, and select a model in the chat bar. Provider keys are stored in the Linux desktop keyring; AgentDeck passes the selected provider settings to Claude Code for that run without editing Claude Code's global config. OpenAI-only endpoints are not compatible with Claude Code. A provider test sends a short request that may incur usage charges. The Debian package includes `curl` as a dependency for provider testing and model discovery. If Claude Code environment variables are set only in a shell, launch `agentdeck` from that shell so it inherits them.
