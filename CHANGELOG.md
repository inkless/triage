# Changelog

## [0.4.0](https://github.com/inkless/triage/compare/v0.3.0...v0.4.0) (2026-06-05)


### Features

* relocate reply to a visible compose surface (panel or bottom bar) ([#5](https://github.com/inkless/triage/issues/5)) ([4439251](https://github.com/inkless/triage/commit/4439251f389cf3ccdcb09de4b729b7f360f7935e))

## [0.3.0](https://github.com/inkless/triage/compare/v0.2.0...v0.3.0) (2026-06-05)


### Features

* live preview rail (p, right/bottom) + l audit-log alias ([#3](https://github.com/inkless/triage/issues/3)) ([82a7a9b](https://github.com/inkless/triage/commit/82a7a9b812c68335abd4b3a676172d6cb1f60ae1))


### Bug Fixes

* drop Claude --bg-spare daemon sessions from discovery ([#2](https://github.com/inkless/triage/issues/2)) ([ab18f74](https://github.com/inkless/triage/commit/ab18f74eaf86a7a6317fdd94daf58d64f4ee915e))

## [0.2.0](https://github.com/inkless/triage/compare/v0.1.0...v0.2.0) (2026-06-04)


### Features

* ^J/^K nav in filter mode (vim-style alt to ↑↓) ([18a71df](https://github.com/inkless/triage/commit/18a71dffeeaff4f1a01e8db59307eeebd8e18289))
* add `triage agents whoami` ([c1e721b](https://github.com/inkless/triage/commit/c1e721be81bd2d7a2a5b23f17233db623d2bcc9f))
* add agent launch CLI ([796a13e](https://github.com/inkless/triage/commit/796a13e757c368cfaab6fc035c285360fd475fdc))
* add guarded agent messaging CLI ([20949b0](https://github.com/inkless/triage/commit/20949b02571ff6b93fea6623181f6982ed65c950))
* add keybinding help overlay ([87e1740](https://github.com/inkless/triage/commit/87e1740fcd4e00997d1d4c28b01c63f52a9d9adf))
* add new agent launch shortcut ([2e79a41](https://github.com/inkless/triage/commit/2e79a412fd14908e5e7593c7c656e772be6fe6e4))
* add tui reply composer ([1633ff0](https://github.com/inkless/triage/commit/1633ff06107b7ec75b0cbf9a8cc8a0e48b37c7a3))
* allow H audit log overlay regardless of auto mode ([65188c4](https://github.com/inkless/triage/commit/65188c4021e91a9170a25610ad67ef417c27aae7))
* auto-detect zoom-on-jump by pane width ([cbd6de4](https://github.com/inkless/triage/commit/cbd6de4ad284be6b74fca9f70350d04e9dd4e948))
* block sends to a target with unsent composer text ([52b4be1](https://github.com/inkless/triage/commit/52b4be1736a3e3a2ecb640b3aae1584f6d552479))
* classify "stuck busy" as Blocked ([a5222be](https://github.com/inkless/triage/commit/a5222befe3ec438052f7a5b510e06865bd09cad7))
* clear filter after a successful Enter-jump ([69906c4](https://github.com/inkless/triage/commit/69906c496c42207e365414baef42a7198e0e3ac7))
* config.toml + ntfy phone push ([64482d1](https://github.com/inkless/triage/commit/64482d1e44b4c5491d47cf8931b6ba47c678d329))
* configure approval mode ([9c2527b](https://github.com/inkless/triage/commit/9c2527b287f2805878f066bebdbc4013c518289f))
* consolidate triage-jump.sh + triage-preuse.sh into the binary ([3b5d067](https://github.com/inkless/triage/commit/3b5d0674bfbbe3affa37919f5664df020b8fce4c))
* cost visibility + full-pane scrape for hook-less audits ([fb4a76f](https://github.com/inkless/triage/commit/fb4a76fe487443e7124f4dbcc0eb5d74148435fc))
* cross-session cost rollup — $ overlay + `triage cost` CLI ([d0cab14](https://github.com/inkless/triage/commit/d0cab14f12498d303deca36cf4b9ddcf76db586c))
* custom AppIcon for triage-notify.app ([d2fd265](https://github.com/inkless/triage/commit/d2fd26521b2f841fb2c98270f0e1889d2fa20bff))
* defer phone push for auto-mode Blocked; fire only on auditor WAIT ([9af9191](https://github.com/inkless/triage/commit/9af919101dfadd0f30ea22982559d6eeb31791f0))
* detail pane — context-window % and agent's latest text ([0dbeb1b](https://github.com/inkless/triage/commit/0dbeb1becbaacdd960fd7bc236806012a294b3db))
* Enter in filter mode jumps directly (single-keystroke confirm + go) ([6c8f33a](https://github.com/inkless/triage/commit/6c8f33a275f3f08104bc706fede655e6a6291c82))
* error on unrecognized args instead of launching TUI ([662189d](https://github.com/inkless/triage/commit/662189d9c758d917f90b37e32c6f8cae7957783b))
* feed recap context to auditor + loosen prompt to allow routine repo ops ([cc97594](https://github.com/inkless/triage/commit/cc9759469baaa6864bd76c7c80a08dd86f726fd4))
* filter-mode nav + readline keys (^W / ^U) ([17bb9b9](https://github.com/inkless/triage/commit/17bb9b9338ac0388dc0b3823872e6c4d2b7a9b09))
* H toggles audit-log overlay (auto-mode history) ([736af36](https://github.com/inkless/triage/commit/736af369562dd9b8b09b218fb5099e7b3a308a68))
* n/N priority hop, popup-mode launch, drop filter ([59564c0](https://github.com/inkless/triage/commit/59564c0c32d4bf28d247d4b814c1118fe484ac59))
* pin sessions to the top with `*` ([2bdb849](https://github.com/inkless/triage/commit/2bdb849d8f935405024c73dd545b35b15349d3e5))
* replace terminal-notifier with triage-notify Swift helper (UNUserNotificationCenter) ([b23ba4c](https://github.com/inkless/triage/commit/b23ba4c18ad4c7d6399a0f6b74a27d982f0d0a9f))
* restore / filter (matches session name + cwd) ([4c40475](https://github.com/inkless/triage/commit/4c40475cbc785dc85ffb1c637275da1377834ca8))
* richer detail pane (full tool_input, auditor in-flight, event timing) ([db322e1](https://github.com/inkless/triage/commit/db322e1fcb1391f0d60a4e68400822bcbd72e092))
* runtime toggle for ntfy phone push (key `p`) ([3cdf288](https://github.com/inkless/triage/commit/3cdf288de43b3b4f7650569abb6f6182dc152de9))
* show auto mode in header ([555b360](https://github.com/inkless/triage/commit/555b360e1eb2ecdbcd7e9164ee14a9121998f73f))
* show Codex tokens in cost overlay ([47260f5](https://github.com/inkless/triage/commit/47260f563ff7cb940557dab9bffd3a31a2e38212))
* show header mode status ([088a275](https://github.com/inkless/triage/commit/088a27534b87e5e6a1c04752d68c41ebb6b6aa46))
* show model name + (1M) tag in detail header (deterministic via settings.json) ([2369ba4](https://github.com/inkless/triage/commit/2369ba4cb473ec1b96edcfa3c9da78d49c6c084b))
* signal accuracy, perf, mute persistence, notifications, approve/deny ([7ff9c54](https://github.com/inkless/triage/commit/7ff9c549b92af25c21266d67719e7919ce5b6724))
* silent attach + tombstone pane_id (phase 1 robustness) ([f4ea7d5](https://github.com/inkless/triage/commit/f4ea7d5bcec15c9ded81f21935f5a9f13ce1449e))
* split exit/zoom flags + uniform selected-row highlight ([e51c7f3](https://github.com/inkless/triage/commit/e51c7f3f3422b560f5ab06ae34a5d559434cdce6))
* support Codex sessions in triage ([c601db1](https://github.com/inkless/triage/commit/c601db1dfe0128fdf8ccaac8d9f86b15c9510556))
* T-51 — idempotent hook installer with dry-run + uninstall ([493ca18](https://github.com/inkless/triage/commit/493ca18a52decd740dd547c9c620319580d7631f))
* T-56 autonomous mode + SESSION tmux-name fallback ([1edbf0a](https://github.com/inkless/triage/commit/1edbf0a2f457b8fba178602b7c3d80a2709ad28e))
* thin divider between pinned and other sessions ([22d5c54](https://github.com/inkless/triage/commit/22d5c545e09ba9c3c49c584f2212a2e2ba90dfe8))
* top-level --help / -h handler ([b3cba82](https://github.com/inkless/triage/commit/b3cba8265547417cac8616755b974d6e5cf10348))
* triage notify &lt;message&gt; — one-shot ntfy push CLI ([7c05020](https://github.com/inkless/triage/commit/7c05020593d4c9a14ae02afb4304e37830d2da3a))
* triage notify fires desktop banner + phone by default ([c15bb8b](https://github.com/inkless/triage/commit/c15bb8b133b0f634634454140f84e6a38ddcc0ea))
* vim keys + cache for audit-log overlay ([d412351](https://github.com/inkless/triage/commit/d41235183ae7c02cb810b1ec8c0b93173af8e0b0))
* watch-session keybinding (w) — fire on every work → done ([babec73](https://github.com/inkless/triage/commit/babec73f34d111232ca138eacef656ce81b68336))


### Bug Fixes

* add tmux discovery fallback ([154840f](https://github.com/inkless/triage/commit/154840f4f8394273533090bd320db90539dd37a5))
* auto-zoom by client width, not pane width ([c5fa202](https://github.com/inkless/triage/commit/c5fa2024a01a0f70446395331157bc2aabe7fc6c))
* autonomous mode plumbing (prompt fallthrough, full tool_input, hook claim handshake) ([7c23474](https://github.com/inkless/triage/commit/7c23474006affc5d9e4f620b784b7a2335a70471))
* bump auditor --max-budget-usd 0.05 → 1.00 ([ab35a3a](https://github.com/inkless/triage/commit/ab35a3aed96d56a8f876625bf4964cae8f002268))
* collapse duplicate sessions sharing a tmux pane ([efd8af4](https://github.com/inkless/triage/commit/efd8af41c62af1cc2d40d1b961ab3cb88adb6235))
* dedupe session cost by message.id (~2-3x overcount) ([6579966](https://github.com/inkless/triage/commit/6579966aaaa96ba75f7f0ab29cabec95ebb56f35))
* detect 1M context window (env var, model tag, or peak observation) ([d29e4d4](https://github.com/inkless/triage/commit/d29e4d4e56a60cb6f77ba89fccff62b1ecb9667c))
* deterministic Blocked via pane content; revert stuck-busy time rule ([cbfd3e0](https://github.com/inkless/triage/commit/cbfd3e093489a5b86d208300538abd1cfdaea889))
* don't treat faint placeholder text as unsent input ([51625a2](https://github.com/inkless/triage/commit/51625a21114b19762babac99f7d15a1653d3fd1b))
* drop literal "1" from approve send-keys (Enter is enough) ([1f25639](https://github.com/inkless/triage/commit/1f25639e5cb6dae648108b2fb1e7456702a75263))
* filter useless window_name labels ([tmux] / nvim / fish / etc.) ([0962f9b](https://github.com/inkless/triage/commit/0962f9b4c205d027ac3d215307f274d2aed79c9b))
* harden blocked-session approval flow ([ec5ff7c](https://github.com/inkless/triage/commit/ec5ff7cf9b3101a1f32ce818cb8f3fdff410fa4d))
* honor sessions JSON status=busy in prompt-freshness gate ([c9a875a](https://github.com/inkless/triage/commit/c9a875a9b4a2f06190f75bd1dada537f4de3b391))
* keep Codex aliases across child threads ([ad7c4bc](https://github.com/inkless/triage/commit/ad7c4bc8d459881e821810781976b8d675047778))
* locate running triage by pid+pane_id, not command name ([bc1ea6f](https://github.com/inkless/triage/commit/bc1ea6f35a3ed4ec1272a07df5b932d454e15b0e))
* make pin and mute mutually exclusive ([b388ce6](https://github.com/inkless/triage/commit/b388ce6ad3fdf3d897c8fd2467ca17bf74cc4a23))
* notification click — quote bundle ID in AppleScript activate ([73662a2](https://github.com/inkless/triage/commit/73662a2840b0cae2e81e55a13db8e7fd2f7b3243))
* notification click — unset TMUX + use open -b for activate ([0d692fc](https://github.com/inkless/triage/commit/0d692fc011554cb617035f7b6c58f60efb6fdf14))
* NSApp-based notify helper — clicks land + no "not responding" dialog ([442d2aa](https://github.com/inkless/triage/commit/442d2aab023b4237150ba60f38ada492a8e704e6))
* pair transcripts by sessionId first, greedy as fallback ([a5d7477](https://github.com/inkless/triage/commit/a5d74770b35e116f35af425536b44a35433e9fbf))
* PID-based triage-pane detection in --jump-to-self ([6885fcc](https://github.com/inkless/triage/commit/6885fcc3eb6a344a30ee50efc1f5dfa1eda0323e))
* pin window in focus_and_maybe_zoom too ([48a4238](https://github.com/inkless/triage/commit/48a42383ad70b172928c4f2acf8d7b21da886df8))
* preserve aliases across state saves ([ed38f50](https://github.com/inkless/triage/commit/ed38f5032c7c1d8a37c131e880a7fc7cda072e02))
* record pane_id in .alive + reuse pane via respawn-pane on stale exit ([171730b](https://github.com/inkless/triage/commit/171730b3e7043a6afc891c795645bb023f93724c))
* reuse running triage pane ([b0c5aa8](https://github.com/inkless/triage/commit/b0c5aa8ce7c7ed017606e721c9c5c6b9ba31cf11))
* send-gate denies only no-pane or visible prompt ([e59c562](https://github.com/inkless/triage/commit/e59c562bc142652fd58855eb16ff262f75bfc06c))
* share agent display labels ([08e1151](https://github.com/inkless/triage/commit/08e11514d0aeee316526a8ed3d123689838894b5))
* stage triage-notify.app to ~/.config/triage so cargo-installed binary finds it ([489076b](https://github.com/inkless/triage/commit/489076bef37bc0dc8925ba8fea86ae73912f9789))
* start rename aliases empty by default ([99dda32](https://github.com/inkless/triage/commit/99dda32aecaf3d5463903e3dc3199df1b4ac8abb))
* surface `w` watch shortcut in the footer hint ([ff917e0](https://github.com/inkless/triage/commit/ff917e0f11e1262aa2b9b68036a635009b6f50d0))
* **triage-notify:** launch via open -na + two-mode helper for click delivery ([6645f00](https://github.com/inkless/triage/commit/6645f0098d6fa9e675a82fe8c0040c1e4e758318))
* tune zoom-on-jump for iPad + pin window in jump_to ([e4649ac](https://github.com/inkless/triage/commit/e4649acc81419931eeeb92dcc8f0f30e894585eb))
* **ui:** row highlight uses REVERSED modifier instead of bg(DarkGray) ([6c5168f](https://github.com/inkless/triage/commit/6c5168fa6b45a701a6a34a4db73fac0e0297ec9f))
* use fleet-wide peak for 1M context detection ([09156c3](https://github.com/inkless/triage/commit/09156c30f2036b33c2550c30d35b1a3a38c8b631))
* use tmux window_name as session-label fallback ([ba50f89](https://github.com/inkless/triage/commit/ba50f89de9deb8fd98685ccd5562a4863da94c23))
