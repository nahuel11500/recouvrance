# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.5.0](https://github.com/nahuel11500/recouvrance/compare/v0.4.0...v0.5.0) - 2026-10-29

### Added

- *(ingress)* Open an Ingress's hosts in the browser (#612)

### Fixed

- *(ingress)* Hosts are links in the Ingresses table too (#615)

## [0.4.0](https://github.com/nahuel11500/recouvrance/compare/v0.3.0...v0.4.0) - 2026-10-29

### Added

- *(confirm)* A short wait instead of typing names to confirm (#589)
- *(problems)* ExternalSecrets and secret stores that fail to sync, and where a Secret comes from (#581)
- *(problems)* Objects stuck deleting, with the finalizers holding them (#582)
- *(alerts)* Firing Prometheus alerts on detail pages and the Problems page (#584)
- *(argo)* Argo CD details on a namespace's page and on objects it deploys (#587)
- *(writes)* Run one write action as another identity (kubectl --as) (#585)
- *(agents)* Explain, can-i, version and wait (#555)
- *(agents)* Diff a manifest before applying it (#557)
- *(agents)* Set an image, pause and resume rollouts (#561)
- *(agents)* Logs of all of a workload's pods (#559)
- *(agents)* Read Helm releases, history and values (#553)
- *(agents)* Cordon, uncordon and top nodes (#549)
- *(agents)* Port-forwards through the MCP server (#545)
- *(agents)* Run a CronJob now and debug a pod (#551)
- *(agents)* Rollout history, status and undo (#547)

### Documentation

- Run as, argocd_contexts and the v0.4.0 features (#605)
- The agents' tools, grouped (#564)

### Fixed

- *(problems)* Scan each cluster for the kinds it serves only (#607)
- *(alerts)* Ask Alertmanager only for held alerts and say when silences are unknown (#609)
- *(overview)* Pods tables show kubectl's status, not the phase (#608)
- *(overview)* No usage bar for a pod without request or limit (#606)
- *(shell)* Readable exit reason and a hint when the image has no shell (#604)
- *(problems)* Keep the kind label on one line (#603)
- *(shell)* Exec in the pod's default container and let the user pick another (#593)
- *(ctx-picker)* One list with recent first for few contexts, no context listed twice (#592)
- *(pages)* Workloads, Problems and the assistant follow a selection of several namespaces (#577)
- *(helm)* List the releases of the namespaces selected, and again when they change (#575)
- *(assistant)* One centered chat column, code blocks at the text's size (#570)
- *(table)* A running pod not ready for 5 minutes shows red (#568)
- *(logs)* Select log lines with the mouse and copy them (#542)
- *(overview)* Sort the pods on a node, and show their usage (#543)

## [0.3.0](https://github.com/nahuel11500/recouvrance/compare/v0.2.0...v0.3.0) - 2026-10-26

### Security

- *(helm)* Mask secret environment variables in values (#497)
- *(settings)* Leave a settings file that can't be read alone and protect every context (#459)
- *(settings)* Keep the protection choices that read when one doesn't (#455)
- *(kubeconfig)* Keep each renamed context name with its file (#444)
- *(agents)* A running drain_node keeps pods a new Deny covers (#464)
- *(nodes)* Delete the node shell pod when its create request fails after the server stored it (#436)
- *(kubeconfig)* Keep a context name with a file caught half-written (#427)
- *(yaml)* Read numbers with a leading zero as octal, as kubectl does (#416)
- *(agents)* Apply_manifest refuses an object whose rule turned into Ask (#458)
- *(agents)* An approved rule covers only the kind and names approved (#450)
- *(agents)* A rule missing a field never grants (#442)
- *(agents)* Refuse a rules.yaml with an unknown key (#415)
- *(access)* Count a namespace's RoleBindings in who can for that Namespace (#424)
- *(agents)* Approving find_problems shows only pod findings unless an Allow rule covers the rest (#414)
- *(helm)* Mask lists of secrets in release values (#445)
- *(agents)* Drain_node names only the pods the agent may list (#422)
- *(agents)* Refuse a rules.yaml with a misspelt key, and don't write over one just saved in an editor (#465)
- *(helm)* Treat --dry-run=false and --dry-run=none as real runs in the write gate (#441)
- *(settings)* Keep a built-in kind's delete choice off custom resources of the same name (#429)
- *(plugins)* Check write mode when a writing plugin is confirmed, and don't re-run it from its terminal (#420)
- *(settings)* Keep protection and agent access safe when a config file can't be read (#354)
- *(agents)* Take away the rules of an agent rules.yaml no longer names, and keep a deleted agent deleted (#352)
- *(mcp)* Reconnect an agent's context when the kubeconfig re-points it, and refuse a removed one (#347)
- *(agents)* Refuse two agents with the same name, which rules.yaml can't tell apart (#343)
- *(agents)* Sum usage_history only over the pods the agent may list (#353)
- *(agents)* Check cluster-scoped lists against rules for every namespace (#350)
- *(agents)* Don't list or contact a context a Deny of everything covers (#346)
- *(agents)* Show describe_resource's events only by the names the agent may list (#345)
- *(agents)* Apply a namespace's Deny and Ask rules to writes on the Namespace itself (#340)
- *(agents)* Mark approved rules and don't keep the flag on pasted rules (#351)
- *(agents)* Apply rule changes made while a call waits for approval (#349)
- *(agents)* Don't turn a certificate scan approval into a rule for every Secret (#344)
- *(agents)* Don't offer a rule for a patch or scale approval (#341)
- *(agents)* Don't turn an all-namespaces approval into a rule (#339)
- *(logs)* Save logs and CSV exports privately, never over an earlier file (#337)
- *(writes)* Drop a write when write mode is turned off during its dry run or chart lookup (#348)
- *(terminal)* Check write mode on every shell and attach, debug and node shells included (#342)
- *(agents)* Don't apply a rule that rules.yaml can't hold (#335)

### Added

- *(assistant)* Investigate a diagnosis with the assistant, with the team's notes and code folders (#396)
- Pod CPU and memory as bars against their request and limit (#226)
- Settings → API requests, how long each cluster takes to answer (#27)
- Show recently visited lists at once while they reload (#26)

### Changed

- The sidebar's render in parts (#532)
- The container page in parts (#531)
- The problem badge and the palette's index as their own types (#529)
- The main window's render in parts (#530)
- One method per Settings section (#528)
- Terminal sessions as a Session type, and Helm storage out of the UI (#527)
- The live list's state in a LiveList, testable without the UI (#526)
- A Verb type for agent requests, and an enum for the assistant backend (#525)
- Split the agents' module into policy, config, audit and hub (#524)
- One place for the app's paths and for private files (#523)
- Idiomatic clippy lints beyond the defaults, and their fixes (#522)
- Explicit imports instead of module-wide globs, and dev-only experiments (#521)

### Documentation

- Fix intra-doc links broken by the mcp module split (#538)
- Match the agents, settings, usage, README and release pages to the code (#519)
- *(agents)* Say agents follow only their rules, not write mode or protection (#511)

### Fixed

- *(lists)* One namespace that fails no longer fails the kind's counts and checks (#513)
- *(logs)* Keep a pod's log lines when its first read fills in the containers (#507)
- *(details)* Keep a slow read of a deleted object off the page (#502)
- *(settings)* Save typed settings when typing pauses, not on every key (#520)
- *(kubeconfig)* Stop adding a subscription each time the path or paste field opens (#518)
- *(kubeconfig)* Give up a context name only after its file stays unchanged for two seconds (#512)
- *(kubeconfig)* Keep a context's name@file name when the bare name is freed (#506)
- *(settings)* Keep every context protected after a restart when the protection choices were lost (#501)
- *(nodes)* Don't flag healthy k3s and RKE2 servers as having a problem (#515)
- *(palette)* Don't show indexing when no searched kind is served (#509)
- *(details)* Delete the object shown when the dialog opened (#498)
- *(mcp)* Say when describe_resource could not list its Pods or Events (#517)
- *(mcp)* Say list_contexts could not read the kubeconfig instead of "no rules" (#516)
- *(agents)* Stop reloading an unreadable rules.yaml every 2 seconds (#514)
- *(agents)* Show the rule in force after a rule edit is refused (#510)
- *(agents)* Keep rules.yaml and the rules in force the same when a reload meets an app edit (#505)
- *(agents)* Keep the rules of an agent 0.2.0 left out of rules.yaml (#504)
- *(helm)* Say write mode is off when a rollback, uninstall or install is confirmed (#503)
- *(helm)* Say which revision can't be read when it blocks an upgrade (#500)
- *(agents)* Switching an agent off works when rules.yaml can't be written (#446)
- *(debug)* Open a debug container's shell in its own tab, keeping the pod's shell (#432)
- *(files)* Ask for write mode before downloading from a container, and say when the folder check fails (#466)
- *(lists)* Keep the stale-list notice while a cluster still shows old rows (#439)
- *(conditions)* Color unknown condition types by what their name says (#447)
- *(problems)* Don't show the all-clear when a cluster couldn't be scanned (#421)
- *(settings)* Say why an MCP port is not saved (#463)
- *(contexts)* Save namespace choices under the cluster that connected first (#462)
- *(files)* Stream an upload from the disk instead of reading it whole (#461)
- *(problems)* Count the sidebar badge in a cluster's accessible namespaces (#460)
- *(certs)* Say how long ago a certificate expired in whole days that went by (#457)
- *(status)* Treat pods held by scheduling gates as waiting, not failing (#452)
- *(helm)* Refuse to upgrade from a release page that is out of date (#448)
- *(nodes)* Flag unknown node conditions that read True (#443)
- *(cluster)* Show what could be read when one part of the summary fails (#435)
- *(navigation)* Don't keep a kind the new clusters don't serve in another tab (#434)
- *(assistant)* Tell the assistant about the current namespace when its page is reused (#426)
- *(logs)* Read a pod's own logs from its first container before the list arrives (#425)
- *(tabs)* Start the watch once when switching tabs (#423)
- *(palette)* Keep the highlight on its item as results stream in (#418)
- *(problems)* Show what other kinds found when one can't be listed (#417)
- *(namespaces)* Refuse a namespace name Kubernetes rejects (#413)
- Stop a drain when agent access or write mode goes off, and test the write guards on kind in every PR (#468)
- *(table)* Keep the highlight on its object when the columns change (#454)
- *(table)* Show each cluster's cells under the right headers when their columns differ (#451)
- *(table)* Stop linking Issuer and Store cells, which may name a cluster-wide kind (#438)
- *(access)* Leave name-restricted rules out of who can for create and deletecollection (#433)
- *(access)* Find the group of a resource typed without one in who can (#428)
- *(audit)* Log the rule that allowed a call and why an approval failed (#453)
- *(agents)* Warn about rules on the contexts protected in Settings, and say when the kubeconfig can't be read (#440)
- *(terminal)* Send Ctrl, Alt and Shift with arrows, editing and function keys (#467)
- *(lists)* Give up a list whose response stops coming for a minute (#449)
- *(mcp)* Say which kinds search couldn't list instead of reporting nothing found (#431)
- *(events)* Show a re-created object's own events and logs (#419)
- *(logs)* Stop following a container that has ended for good (#456)
- *(revisions)* Say why a StatefulSet's or DaemonSet's revisions could not be listed (#437)
- *(palette)* Drop the last palette's subscription when it opens again (#338)
- *(sidebar)* Show "Connecting to" in the header while a context connects (#336)
- *(access)* Give each subject its own valid binding name when binding a role (#329)
- *(details)* Keep reading a page whose first read failed (#326)
- *(namespaces)* Keep accessible namespaces to the cluster they are set for (#282)
- *(settings)* Show settings changed in another window (#332)
- *(settings)* Apply a per-kind delete confirmation typed in lowercase or plural (#330)
- *(palette)* Keep the highlighted row in view (#323)
- *(theme)* Follow a live desktop light/dark switch with System (#320)
- *(terminal)* Send Home/End in application-cursor mode and the function keys (#277)
- *(mcp)* Approving a one-namespace list shows the rows of every Ask rule it meets (#334)
- *(agents)* Refuse agent-create while the app or serve is running (#331)
- *(access)* Say when My access may be incomplete (#327)
- *(access)* Say which RBAC lists could not be read instead of showing nobody (#318)
- *(access)* Who can skips RoleBindings for cluster-scoped resources (#293)
- *(kubeconfig)* Keep each context name on the file that had it first (#333)
- *(navigation)* Clear the list's filter when a link opens a kind's list (#328)
- *(tabs)* Keep the highlight on the selected object when switching tabs (#324)
- *(contexts)* List namespaces of all clusters at once, skipping unreachable ones (#321)
- *(multi-cluster)* Clear a cluster's rows when it refuses the list (#317)
- *(multi-cluster)* Drop cached rows of a cluster that no longer serves the kind (#315)
- *(lists)* Stop the paged list when nobody waits for it (#312)
- *(files)* Show why an upload failed instead of "broken pipe" (#288)
- *(node-shell)* Delete the privileged pod when waiting for it fails (#286)
- *(metrics)* Count sidecars, overhead and pod-level resources in the pod usage bars (#280)
- *(logs)* Read the default container when the container list fails (#325)
- *(logs)* Keep JSON values with newlines on one row (#319)
- *(events)* Show the count and time of repeated events in the Events tab (#310)
- *(logs)* Keep every line of a followed log at batch boundaries (#281)
- *(logs)* Reconnect a followed stream that goes silent (#276)
- *(certificates)* Show a certificate that is not valid yet as not valid (#322)
- *(network)* Show NetworkPolicy port ranges (#303)
- *(dashboard)* Don't count a disruption budget without pods as bad (#287)
- *(gateway)* Show each route parent's own status (#283)
- *(pdb)* List no pods for a budget without a selector (#278)
- *(status)* Show ContainerStatusUnknown as failed in tables (#316)
- *(usage)* Leave finished pods out of a workload's usage charts (#313)
- *(diagnosis)* Don't report an evicted pod's containers as failed (#299)
- *(overview)* Count init and sidecar containers in the cluster's requested resources (#285)
- *(metrics)* Stop reading metrics-server when metrics are off (#275)
- *(schema-form)* Add an item to a list that is empty or null in the YAML (#311)
- *(helm)* Open the install page from the release list after F5 (#284)
- *(helm)* Show a dry run or install result only under its own chart (#279)
- *(helm)* Keep values typed while the chart's values load (#274)
- *(usage)* Compare workload usage with the requests of the same pods (#201)
- *(details)* Keep the chosen time range on the usage charts (#195)
- *(network)* List the pods a NetworkPolicy applies to (#192)
- *(logs)* Keep the view on the same lines with Follow and Wrap off (#207)
- *(logs)* Follow at most 50 pods on one page (#223)
- *(logs)* Show logs of pods that lack the chosen container (#212)
- *(discovery)* Say so when the core group fails instead of hiding Pods and Services (#221)
- *(discovery)* Show kinds served only outside a group's preferred version (#217)
- *(lists)* Stop an abandoned whole-list read instead of letting it download (#211)
- *(watch)* Resume a cut watch instead of relisting the whole kind (#198)
- *(assistant)* Keep a question from hanging, and add Stop (#210)
- *(assistant)* Answer a question that starts with "-" (#205)
- *(mcp)* Show every approved namespace's rows in an all-namespaces list (#200)
- *(mcp)* Log a rejected all-namespaces list as rejected, and show nothing when no namespace is allowed (#193)
- *(table)* Keep the highlight on the selected object after a filter, sort or metrics update (#227)
- *(contexts)* Drop the old clusters' watch errors when switching contexts (#224)
- *(contexts)* Keep the list's columns when switching contexts and back (#222)
- *(contexts)* Report a cluster that can't be reached when connecting to several (#219)
- *(navigation)* Go back to a list the new clusters have after a context switch (#216)
- *(table)* Keep a check made during the fast first load on its own cluster (#203)
- *(access)* Give each ServiceAccount its own binding name when binding a role (#225)
- *(rbac)* Stop treating pods/* as every subresource in Who can (#220)
- *(palette)* List each object once when the search index is built (#218)
- *(workloads)* Don't warn about drains for a PodDisruptionBudget that covers no pods (#214)
- *(port-forward)* Open the right URL for an IPv6 listen address (#208)
- *(overview)* Stop drawing healthy False conditions in red (#202)
- *(dashboard)* Count repeating warnings recorded as an event series (#196)
- *(kubeconfig)* Keep the current context on its own file's cluster when names clash (#197)
- *(contexts)* Drop ticks on contexts removed from the kubeconfig (#206)
- *(kubeconfig)* Keep existing context names when a new file is pasted or dropped in a folder (#215)
- *(kubeconfig)* Pick up a pasted kubeconfig on the first reload (#209)
- *(helm-install)* Show only the latest chart search and the picked chart's versions (#204)
- *(field-docs)* Read documents after `--- # comment` with their own schema (#199)
- *(schema-form)* Keep the final line break when editing a one-line script (#194)
- *(helm)* Open the newest revision when the release list is out of date (#191)
- *(mcp)* Show a just-expired certificate as expired in the certificates tool (#153)
- *(settings)* Refresh Per cluster when the kubeconfig reloads, in kubeconfig order (#152)
- *(namespaces)* Keep several namespaces ticked when all are accessible (#151)
- *(table)* Show "Loading…" while a list loads instead of "No resources" (#95)
- *(table)* Keep checks, selection and the row menu made during the fast first load (#131)
- *(multi-cluster)* Keep showing which contexts failed to connect or list (#128)
- *(table)* Keep the scroll position when rows move under the selection (#121)
- *(table)* Say a list is forbidden only when every cluster refused it (#111)
- *(table)* Clear the highlighted row on a kind switch so Enter doesn't open another object (#106)
- *(settings)* Two saves at the same moment no longer lose a change (#146)
- *(helm)* One cluster's error no longer hides the other clusters' releases (#141)
- *(helm)* After a rollback or upgrade, the release page shows the new revision (#135)
- *(helm)* Picking charts quickly no longer installs one chart with another's values (#129)
- *(discovery)* Resolve svc, roles and other built-in names to the built-in kind (#126)
- *(discovery)* List the other kinds when one API group does not answer (#118)
- *(kubeconfig)* Show the real renamed context names before saving a paste (#105)
- *(form)* Writing no longer deletes the next document after a --- # comment line (#122)
- *(kubeconfig)* Keep a typed relative path working from any directory (#102)
- *(form)* Write to YAML keeps multi-line strings (#116)
- *(terminal)* Trackpad scrolling moves through the history (#150)
- *(serviceaccounts)* Keep tls-server-name in the downloaded kubeconfig (#99)
- *(agents)* List the contexts an agent may use the way the rules match them (#138)
- *(terminal)* Ctrl+Shift+C no longer interrupts the running process (#149)
- *(delete)* Remove finalizers only when the object stays stuck terminating (#148)
- *(port-forward)* Show why the pod refused a connection (#104)
- *(attach)* Keep multi-byte characters whole in output without a tty (#100)
- *(kubeconfig)* Keep other clusters responsive while an exec plugin logs in (#96)
- *(assistant)* Stop using a revoked token after "New token" on the Claude agent (#93)
- *(audit)* Record which objects apply_manifest applied (#145)
- *(agents)* Refresh the rule editor when rules.yaml changes, and say the page writes to it (#140)
- *(agents)* Show the allowed namespaces of an all-namespaces list when others ask (#134)
- *(apply)* Opening a file or loading a URL no longer cancels a running apply (#90)
- *(exec)* Attach to the chosen container, debug and init ones too (#94)
- *(logs)* Show the container-list error and try again (#119)
- *(logs)* Keep the view and find match on their lines when pods' lines merge (#124)
- *(compare)* List candidates in the object's namespace and show list errors (#125)
- *(details)* Show events for objects opened by name (#112)
- *(access)* Keep * in the core group from counting as every group in My access (#137)
- *(access)* Read deployments.apps/scale in Who can as a subresource (#147)
- *(access)* Bind a ClusterRole into a namespace (#133)
- *(dashboard)* Match container usage to pods by namespace too (#103)
- *(kubeconfig)* Keep merged names unique when several files share a name (#92)
- *(diagnosis)* Report containers that stopped with any failure (#107)
- *(certificates)* Show a just-expired certificate as expired (#120)
- *(cronjobs)* Show each Job's real state from its conditions (#110)
- *(logs)* Show workload logs for pods with several containers (#109)
- *(dev)* Don't crash the snapshot tool when RECOUVRANCE_FRAMES_END=0 (#143)
- *(overview)* Show ReplicaFailure, FailureTarget and DisruptionTarget as bad when true (#123)
- *(problems)* Count a restart as recent only within the last hour (#130)
- *(filter)* Filter on hidden columns too (#101)
- *(agents)* A Deny rule for some names hides those rows instead of refusing the whole list (#132)
- *(assistant)* Show why the API key could not be saved (#144)
- *(settings)* List contexts added since the window opened under Per cluster (#142)
- *(theme)* Keep following the desktop after choosing System (#139)
- *(namespaces)* Keep the namespace just ticked when only some are accessible (#136)
- *(sidebar)* Drop the old clusters' problem count when switching contexts (#117)
- *(palette)* List same-named objects from every connected cluster (#113)
- *(agents)* Make "Always allow this" and "Allow for 1h" stop the same request from asking again (#127)
- *(contexts)* Ticking a context in the picker adds it instead of switching to it (#108)
- *(assistant)* Keep the chat working when a reply is cut off during a tool call (#115)
- *(files)* Downloading / from a container saves the whole filesystem (#98)
- *(network)* Show NetworkPolicy selectors that use matchExpressions (#114)
- *(watch)* Back off when a watch keeps failing instead of relisting every second (#91)
- Recover from dead connections, back off on errors, and list less (#24)

### Performance

- Show Helm releases and the cluster overview in seconds (#97)
- Faster lists and smooth scrolling on big clusters (#29)

## [0.2.0](https://github.com/nahuel11500/recouvrance/compare/v0.1.0...v0.2.0) - 2026-10-22

### Added

- Delete confirmation per kind, with defaults by how much a kind matters ([#16](https://github.com/nahuel11500/recouvrance/pull/16))
- A simpler app icon for 16 and 24 px ([#15](https://github.com/nahuel11500/recouvrance/pull/15))
- New app icon, with a README trust section, screenshots and tour, pinned actions and Scorecard ([#14](https://github.com/nahuel11500/recouvrance/pull/14))
- *(agents)* Activity grouped by task, with filters and older calls ([#13](https://github.com/nahuel11500/recouvrance/pull/13))
- *(pods)* Port-forward from the Ports cards ([#10](https://github.com/nahuel11500/recouvrance/pull/10))
- *(nodes)* Allocation as cards with requests and limits bars ([#9](https://github.com/nahuel11500/recouvrance/pull/9))

### Fixed

- Run each exec-plugin login once at a time ([#11](https://github.com/nahuel11500/recouvrance/pull/11))
- Let dialog inputs keep focus ([#8](https://github.com/nahuel11500/recouvrance/pull/8))
- *(table)* One sort arrow, on the sorted column only ([#6](https://github.com/nahuel11500/recouvrance/pull/6))

## [0.1.0](https://github.com/nahuel11500/recouvrance/compare/v0.0.1...v0.1.0) - 2026-10-20

### Added

- *(namespaces)* Larger picker and frequently visited namespaces ([#4](https://github.com/nahuel11500/recouvrance/pull/4))
- Show the version, add --version and --help, cross-build Intel Macs ([#3](https://github.com/nahuel11500/recouvrance/pull/3))

## [0.0.1]

### Added

- Harbour look, after the schooner Recouvrance: navy hull sidebar, sail-canvas pages, brass accent
  (selection, primary buttons), sea and signal status colors; brass, harbour, sea, kelp, signal and
  violet accents.
- CPU and memory history per container, against its request and limit.
- Secret values revealed one at a time by clicking them, with a copy button.
- One-click port-forwards open the browser on web ports (setting); recent forwards in the sidebar
  start again in one click.
- GPU usage (utilization and memory) next to CPU and memory on pod, workload and node pages and the
  cluster overview, from NVIDIA's DCGM exporter through Prometheus or VictoriaMetrics; GPU requests
  against allocatable GPUs on the cluster overview.
- Diagnosis of broken pods and workloads (crash output, exit codes, scheduling, image pulls,
  probes, volumes).
- Roll back Deployments, StatefulSets and DaemonSets to a revision, with diff and server dry run.
- Attach to a container's process; copy files in and out of containers; bulk scale.
- Edit ConfigMap and Secret values as key/value rows; create Secrets and ConfigMaps from forms.
- RBAC pages (roles, bindings, ServiceAccounts), a who-can inspector, short-lived ServiceAccount
  kubeconfigs; PodDisruptionBudget and VerticalPodAutoscaler pages.
- Node allocation (extended resources included), filesystems, DRA devices; pod ephemeral storage.
- Create / apply: templates and your own templates folder, files, URLs, drag and drop, `$EDITOR`,
  errors on their line, field docs and forms from the cluster's schema.
- Find in logs (regex, match case); exclusions in the log filter.
- Table filter expressions on columns, filter history and saved searches; links from cells to the
  objects they name; tabs; CSV export; click a namespace to filter.
- Port-forward options (local port, listen address, https, open in browser); forwards follow
  replacement pods.
- VictoriaMetrics support; per-cluster settings (metrics endpoint, node shell image, accessible
  namespaces); manually added namespaces.
- Kubeconfig watched folders, reload on change, contexts with the same name kept apart.
- Accent color, metrics off switch, refresh (F5), sidebar toggle (Ctrl+B).

### Changed

- Types you may not list (403) stop retrying and say so; API errors are shown as the API's own message.
- Responses from the API server are gzip-compressed (much faster lists on remote clusters).
- The in-app assistant runs through Claude Code (your subscription) when it is installed, and
  needs an Anthropic API key only without it.
