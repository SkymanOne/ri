// The extension host inside the guest: loads pi extensions, gives each its
// `pi` API object, and runs its handlers, tools, commands and shortcuts when
// the host dispatches them. Port of `core/extensions/loader.ts` and the
// per-extension part of `runner.ts` in pi `v1.0.0`; the host chains results
// across extensions.
(() => {
	"use strict";
	const yapi = globalThis.__yapi;
	const native = globalThis.__yapi_native;
	const EventEmitter = yapi.EventEmitter;

	const extensions = new Map();
	const flagValues = new Map();
	let bound = false;

	const errorMessage = (error) => (error instanceof Error ? error.message : String(error));
	const errorInfo = (error) => ({ error: errorMessage(error), stack: error instanceof Error ? error.stack : undefined });
	const plain = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));
	const orUndefined = (value) => value ?? undefined;

	// ----- event buses --------------------------------------------------------
	/** pi's `createEventBus`. */
	yapi.createEventBus = () => {
		const emitter = new EventEmitter();
		return {
			emit: (channel, data) => {
				emitter.emit(channel, data);
			},
			on: (channel, handler) => {
				const safe = async (data) => {
					try {
						await handler(data);
					} catch (error) {
						console.error(`Event handler error (${channel}):`, error);
					}
				};
				emitter.on(channel, safe);
				return () => emitter.off(channel, safe);
			},
			clear: () => {
				emitter.removeAllListeners();
			},
		};
	};
	// `pi.events`, which every extension shares.
	const eventBus = yapi.createEventBus();

	// ----- runtime actions -----------------------------------------------------------
	const notInitialized = () => {
		throw new Error("Extension runtime not initialized. Action methods cannot be called during extension loading.");
	};
	const action = (kind) => (payload) => (bound ? yapi.request(kind, payload) : notInitialized());

	// ----- the pi API ------------------------------------------------------------------------
	function createApi(extension) {
		const pendingFlagValues = new Map();
		let state = "loading";
		// Every method throws once the extension has failed to load.
		const active = (methods) => {
			for (const [name, method] of Object.entries(methods)) {
				if (typeof method !== "function") continue;
				methods[name] = (...args) => {
					if (state === "failed") throw new Error(`Extension "${extension.path}" failed to load and its API is no longer active.`);
					return method(...args);
				};
			}
			return methods;
		};
		const api = active({
			on(event, handler) {
				const registered = (...args) => handler(...args);
				const list = extension.handlers.get(event) ?? [];
				list.push(registered);
				extension.handlers.set(event, list);
				return () => {
					const handlers = extension.handlers.get(event);
					if (!handlers) return;
					const index = handlers.indexOf(registered);
					if (index !== -1) handlers.splice(index, 1);
					if (handlers.length === 0) extension.handlers.delete(event);
				};
			},
			registerTool(tool) {
				if (typeof tool.parameters !== "object" || tool.parameters === null || Array.isArray(tool.parameters)) {
					throw new Error(`Tool "${tool.name}" registered by extension "${extension.path}" must define an object parameter schema.`);
				}
				extension.tools.set(tool.name, tool);
				if (bound) yapi.request("tools.refresh", { extension: extension.id, tools: [describeTool(tool)] });
			},
			registerCommand(name, options) {
				if (typeof name !== "string" || name.length === 0) {
					throw new Error(`Command registered by extension "${extension.path}" must have a non-empty string name. Use pi.registerCommand("name", { description, handler }).`);
				}
				if (typeof options?.handler !== "function") {
					throw new Error(`Command "/${name}" registered by extension "${extension.path}" must define handler().`);
				}
				extension.commands.set(name, { name, ...options });
			},
			registerShortcut(shortcut, options) {
				extension.shortcuts.set(shortcut, { shortcut, ...options });
			},
			registerFlag(name, options) {
				if (options.default !== undefined && typeof options.default !== options.type) {
					throw new Error(`Invalid default for flag "${name}": expected ${options.type}, got ${typeof options.default}`);
				}
				extension.flags.set(name, { name, ...options });
				if (options.default !== undefined && !flagValues.has(name)) {
					if (state === "loading") {
						if (!pendingFlagValues.has(name)) pendingFlagValues.set(name, options.default);
					} else flagValues.set(name, options.default);
				}
			},
			registerMessageRenderer(customType, renderer) {
				extension.messageRenderers.set(customType, renderer);
			},
			registerMarkdownTransformer(transformer) {
				extension.markdownTransformer = transformer;
			},
			registerEntryRenderer(customType, renderer) {
				extension.entryRenderers.set(customType, renderer);
			},
			getFlag(name) {
				if (!extension.flags.has(name)) return undefined;
				return flagValues.has(name) ? flagValues.get(name) : pendingFlagValues.get(name);
			},
			sendMessage(message, options) {
				action("session.sendMessage")({ message: plain(message), options: plain(options) });
			},
			sendUserMessage(content, options) {
				action("session.sendUserMessage")({ content: plain(content), options: plain(options) });
			},
			appendEntry(customType, data) {
				action("session.appendEntry")({ customType, data: plain(data) });
			},
			setSessionName(name) {
				action("session.setName")({ name });
			},
			getSessionName() {
				return action("session.getName")({}) ?? undefined;
			},
			setLabel(entryId, label) {
				action("session.setLabel")({ entryId, label });
			},
			exec(command, args, options) {
				return execCommand(command, args, options?.cwd ?? yapi.cwd, options);
			},
			getActiveTools() {
				return action("tools.getActive")({});
			},
			getAllTools() {
				return action("tools.getAll")({});
			},
			getSettings() {
				return action("settings.get")({});
			},
			setActiveTools(names) {
				action("tools.setActive")({ names });
			},
			getCommands() {
				return action("commands.list")({});
			},
			setModel(model) {
				if (!bound) return Promise.reject(new Error("Extension runtime not initialized"));
				return yapi.op("model.set", { provider: model.provider, id: model.id });
			},
			getThinkingLevel() {
				return action("thinking.get")({});
			},
			setThinkingLevel(level) {
				action("thinking.set")({ level });
			},
			registerProvider(nameOrProvider, config) {
				if (typeof nameOrProvider === "string") {
					if (!config) throw new Error("Provider config is required when registering by name");
					const provider = { name: nameOrProvider, config: describeProvider(config) };
					// The code stays here; the host gets the description.
					Object.defineProperty(provider, "implementation", { value: config });
					extension.providers.push(provider);
				} else extension.providers.push({ name: nameOrProvider.id, native: true });
			},
			unregisterProvider(name) {
				extension.providers = extension.providers.filter((provider) => provider.name !== name);
			},
			registerMcpServer(name, config) {
				extension.mcpServers.set(name, plain(config));
			},
			unregisterMcpServer(name) {
				extension.mcpServers.delete(name);
			},
			getMcpServers() {
				return [...extension.mcpServers].map(([name, config]) => ({ name, config, extensionPath: extension.path }));
			},
			registerVirtualModel(model) {
				extension.virtualModels.push(model);
			},
			unregisterVirtualModel(provider, id) {
				extension.virtualModels = extension.virtualModels.filter((model) => model.provider !== provider || model.id !== id);
			},
			events: active({
				emit(channel, data) {
					eventBus.emit(channel, data);
				},
				on(channel, handler) {
					return eventBus.on(channel, handler);
				},
			}),
		});
		return {
			api,
			commit() {
				for (const [name, value] of pendingFlagValues) if (!flagValues.has(name)) flagValues.set(name, value);
				state = "active";
			},
			discard() {
				state = "failed";
			},
		};
	}

	async function execCommand(command, args, cwd, options = {}) {
		// As pi's execCommand: a process that cannot start reports code 1.
		const result = await yapi
			.op("exec", { command, args: args ?? [], cwd, timeout: options.timeout, env: options.env, input: options.input })
			.catch(() => ({ stdout: "", stderr: "", code: 1, killed: false }));
		return { stdout: result.stdout, stderr: result.stderr, code: result.code ?? 0, killed: !!result.killed };
	}

	// ----- descriptions sent to the host -----------------------------------------------
	function describeTool(tool) {
		return {
			name: tool.name,
			label: tool.label ?? null,
			description: tool.description,
			promptSnippet: tool.promptSnippet,
			promptGuidelines: tool.promptGuidelines,
			parameters: plain(tool.parameters),
			outputSchema: plain(tool.outputSchema),
			constrainedSampling: tool.constrainedSampling,
			renderShell: tool.renderShell,
			exposure: tool.exposure,
			namespace: tool.namespace,
			annotations: tool.annotations,
			defaultActive: tool.defaultActive,
			executionMode: tool.executionMode,
			hasRenderCall: typeof tool.renderCall === "function",
			hasRenderResult: typeof tool.renderResult === "function",
		};
	}
	function describeProvider(config) {
		const out = {};
		for (const [key, value] of Object.entries(config)) {
			if (typeof value === "function") continue;
			if (key === "oauth" && value) out.oauth = { name: value.name, isSubscription: value.isSubscription };
			else out[key] = plain(value);
		}
		out.hasStreamSimple = typeof config.streamSimple === "function";
		out.hasRefreshModels = typeof config.refreshModels === "function";
		return out;
	}
	/** The configuration of provider `name` as last registered, with its code. */
	function providerConfig(name) {
		let found;
		for (const extension of extensions.values()) {
			for (const provider of extension.providers) if (provider.name === name && provider.implementation) found = provider.implementation;
		}
		return found;
	}
	function describe(extension) {
		return {
			id: extension.id,
			path: extension.path,
			tools: [...extension.tools.values()].map(describeTool),
			commands: [...extension.commands.values()].map((command) => ({
				name: command.name,
				description: command.description ?? null,
				hasCompletions: typeof command.getArgumentCompletions === "function",
			})),
			flags: [...extension.flags.values()].map((flag) => ({
				name: flag.name,
				type: flag.type,
				default: flag.default ?? null,
				description: flag.description ?? null,
			})),
			shortcuts: [...extension.shortcuts.values()].map((shortcut) => ({ shortcut: shortcut.shortcut, description: shortcut.description ?? null })),
			events: [...extension.handlers.keys()].sort(),
			messageRenderers: [...extension.messageRenderers.keys()],
			entryRenderers: [...extension.entryRenderers.keys()],
			markdownTransformer: typeof extension.markdownTransformer === "function",
			providers: extension.providers,
			mcpServers: [...extension.mcpServers].map(([name, config]) => ({ name, config })),
		};
	}

	// ----- loading --------------------------------------------------------------------------------
	async function loadOne({ id, path }) {
		let module;
		try {
			module = await import(path);
		} catch (error) {
			return { id, path, error: `Failed to load extension: ${errorMessage(error)}` };
		}
		const factory = module?.default;
		if (typeof factory !== "function") return { id, path, error: `Extension does not export a valid factory function: ${path}` };
		return instantiate(id, path, factory);
	}

	/** Runs `factory` with a fresh API: pi does this for every session. */
	async function instantiate(id, path, factory) {
		const extension = {
			factory,
			id,
			path,
			handlers: new Map(),
			tools: new Map(),
			commands: new Map(),
			flags: new Map(),
			shortcuts: new Map(),
			messageRenderers: new Map(),
			entryRenderers: new Map(),
			markdownTransformer: undefined,
			providers: [],
			mcpServers: new Map(),
			virtualModels: [],
		};
		const load = createApi(extension);
		try {
			await factory(load.api);
			load.commit();
		} catch (error) {
			load.discard();
			return { id, path, error: `Failed to load extension: ${errorMessage(error)}` };
		}
		extensions.set(id, extension);
		return describe(extension);
	}

	// ----- theme ------------------------------------------------------------------------------------------
	const THINKING_TOKENS = { minimal: "thinkingMinimal", low: "thinkingLow", medium: "thinkingMedium", high: "thinkingHigh", xhigh: "thinkingXhigh", max: "thinkingMax" };
	/**
	 * pi's `Theme` over the escape sequences the host reports for each token
	 * (yapi: `load`). Without them text stays plain.
	 */
	class Theme {
		#fg = {};
		#bg = {};
		#dim = new Set();
		#styled = false;
		#mode = "truecolor";
		constructor(spec) {
			this.load(spec);
		}
		/** Takes the host's `{ name, mode, fg, bg, dim }`, or `null` for plain text. */
		load(spec) {
			this.#styled = !!spec;
			this.#fg = spec?.fg ?? {};
			this.#bg = spec?.bg ?? {};
			this.#dim = new Set(spec?.dim ?? []);
			this.#mode = spec?.mode ?? "truecolor";
			this.name = spec?.name;
		}
		#sequence(map, token) {
			const sequence = map[token];
			if (sequence === undefined) throw new Error(`Unknown theme color: ${token}`);
			return sequence;
		}
		fg(token, text) {
			if (!this.#styled) return text;
			const sequence = this.#sequence(this.#fg, token);
			return this.#dim.has(token) ? `${sequence}\x1b[2m${text}\x1b[22;39m` : `${sequence}${text}\x1b[39m`;
		}
		bg(token, text) {
			return this.#styled ? `${this.#sequence(this.#bg, token)}${text}\x1b[49m` : text;
		}
		#wrap(open, close, text) {
			return this.#styled ? `\x1b[${open}m${text}\x1b[${close}m` : text;
		}
		bold(text) {
			return this.#wrap(1, 22, text);
		}
		italic(text) {
			return this.#wrap(3, 23, text);
		}
		underline(text) {
			return this.#wrap(4, 24, text);
		}
		strikethrough(text) {
			return this.#wrap(9, 29, text);
		}
		inverse(text) {
			return this.#wrap(7, 27, text);
		}
		getFgAnsi(token) {
			if (!this.#styled) return "";
			const sequence = this.#sequence(this.#fg, token);
			return this.#dim.has(token) ? `${sequence}\x1b[2m` : sequence;
		}
		getBgAnsi(token) {
			return this.#styled ? this.#sequence(this.#bg, token) : "";
		}
		getColorMode() {
			return this.#mode;
		}
		getThinkingBorderColor(level) {
			const token = THINKING_TOKENS[level] ?? "thinkingOff";
			return (text) => this.fg(token, text);
		}
		getBashModeBorderColor() {
			return (text) => this.fg("bashMode", text);
		}
	}
	yapi.Theme = Theme;
	const theme = new Theme(null);
	globalThis.__yapi_theme = theme;

	// ----- components ---------------------------------------------------------------------------------------
	// Components live here, by handle; the host renders them through
	// `yapi.render` and delivers keys through `yapi.input`.
	const components = new Map();
	let nextHandle = 1;
	const widgetHandles = new Map();
	const slots = { footer: undefined, header: undefined };
	// Components of transcript items, by tool call or message.
	const transcriptViews = new Map();
	function mount(component) {
		const handle = nextHandle++;
		components.set(handle, component);
		return handle;
	}
	function unmount(handle) {
		const component = components.get(handle);
		components.delete(handle);
		try {
			component?.dispose?.();
		} catch (error) {
			console.error("Component dispose error:", error);
		}
	}
	const tui = {
		requestRender: () => {
			if (bound) yapi.request("ui.requestRender", {});
		},
		terminal: { columns: 80, rows: 24, write() {}, setTitle: (title) => bound && yapi.request("ui.setTitle", { title }) },
		setFocus() {},
		showOverlay() {
			return overlayHandle(undefined);
		},
		hideOverlay() {},
		start() {},
		stop() {},
	};
	function overlayHandle(handle) {
		let hidden = false;
		return {
			hide: () => handle !== undefined && yapi.request("ui.close", { handle }),
			setHidden: (value) => void (hidden = value),
			isHidden: () => hidden,
			focus() {},
			unfocus() {},
			isFocused: () => !hidden,
		};
	}
	let tuiModule;
	let facade;
	const keybindings = async () => {
		tuiModule ??= await import("@earendil-works/pi-tui");
		return tuiModule.getKeybindings();
	};
	const request = (kind, payload) => yapi.request(`ui.${kind}`, payload);
	// pi-tui's input listeners: `onTerminalInput` handlers, which see raw
	// input before the editor and may consume or transform it.
	const terminalListeners = new Set();
	// `addAutocompleteProvider` factories and the provider they compose over
	// the host's, as pi's `setupAutocompleteProvider` does.
	const completionWrappers = [];
	const hostCompletions = {
		getSuggestions: (lines, cursorLine, cursorCol, options) =>
			yapi.op("ui.suggestions", { lines, cursorLine, cursorCol, force: !!options?.force }).then((suggestions) => suggestions ?? null),
		applyCompletion: (lines, cursorLine, cursorCol, item, prefix) => request("applyCompletion", { lines, cursorLine, cursorCol, item: plain(item), prefix }),
	};
	let completions = hostCompletions;
	function addCompletions(factory) {
		completionWrappers.push(factory);
		let provider = hostCompletions;
		const triggers = [];
		for (const wrap of completionWrappers) {
			provider = wrap(provider);
			triggers.push(...(provider.triggerCharacters ?? []));
		}
		if (triggers.length > 0) provider.triggerCharacters = [...new Set(triggers)];
		completions = provider;
		components.get(editorSlot.handle)?.setAutocompleteProvider?.(provider);
		request("setAutocomplete", { triggerCharacters: provider.triggerCharacters ?? [] });
	}
	// The extension editor in the built-in one's place, and the app actions
	// it asks the host for; pi's `setCustomEditorComponent`. The modules it
	// needs load when the session binds, so the factory runs at once.
	const editorSlot = { factory: undefined, handle: undefined };
	let editorActions = [];
	function setEditor(factory) {
		const text = request("getEditorText", {}) ?? "";
		const editor = typeof factory === "function" ? factory(tui, facade.getEditorTheme(), tuiModule.getKeybindings()) : undefined;
		editorSlot.factory = editor ? factory : undefined;
		if (editorSlot.handle !== undefined) unmount(editorSlot.handle);
		editorSlot.handle = undefined;
		if (!editor) {
			request("setEditor", {});
			return;
		}
		editor.onSubmit = (value) => request("editorSubmit", { text: value });
		editor.onChange = () => request("editorChange", { text: editor.getExpandedText?.() ?? editor.getText() });
		editor.setText(text);
		editor.setAutocompleteProvider?.(completions);
		// An editor extending `CustomEditor` triggers the app's actions, as the built-in editor does.
		if (editor.actionHandlers instanceof Map) {
			const action = (id) => () => request("editorAction", { action: id });
			editor.onEscape ??= action("app.interrupt");
			editor.onCtrlD ??= action("app.exit");
			editor.onPasteImage ??= action("app.clipboard.pasteImage");
			editor.onExtensionShortcut ??= (data) => !!request("editorShortcut", { data });
			for (const id of editorActions) editor.actionHandlers.set(id, action(id));
		}
		editorSlot.handle = mount(editor);
		request("setEditor", { handle: editorSlot.handle, embedsStatus: editor.embedWorkingStatus === true });
	}
	yapi.render = (handle, width) => {
		const component = components.get(handle);
		if (!component) return [];
		try {
			return component.render(width).map(String);
		} catch (error) {
			return [`Render error: ${errorMessage(error)}`];
		}
	};
	yapi.input = (handle, data) => {
		try {
			components.get(handle)?.handleInput?.(data);
		} catch (error) {
			console.error("Component input error:", error);
		}
	};

	// ----- contexts --------------------------------------------------------------------------------------
	function createUi(data) {
		const shown = !!(data.hasUI && data.components);
		const replaceSlot = (slot, factory, ...args) => {
			if (slots[slot] !== undefined) unmount(slots[slot]);
			slots[slot] = undefined;
			if (typeof factory === "function" && shown) slots[slot] = mount(factory(tui, theme, ...args));
			if (shown) request(slot === "footer" ? "setFooter" : "setHeader", { handle: slots[slot] });
		};
		const footerData = {
			getGitBranch: () => request("footerData", {})?.gitBranch ?? null,
			getExtensionStatuses: () => new Map(request("footerData", {})?.statuses ?? []),
			getAvailableProviderCount: () => request("footerData", {})?.providers ?? 0,
			onBranchChange: () => () => {},
		};
		return {
			// A cancelled dialog resolves to `undefined`, as in pi.
			select: (title, options, opts) => (data.hasUI ? yapi.op("ui.select", { title, options, timeout: opts?.timeout }).then(orUndefined) : Promise.resolve(undefined)),
			confirm: (title, message, opts) => (data.hasUI ? yapi.op("ui.confirm", { title, message, timeout: opts?.timeout }) : Promise.resolve(false)),
			input: (title, placeholder, opts) => (data.hasUI ? yapi.op("ui.input", { title, placeholder, timeout: opts?.timeout }).then(orUndefined) : Promise.resolve(undefined)),
			editor: (title, prefill) => (data.hasUI ? yapi.op("ui.editor", { title, prefill }).then(orUndefined) : Promise.resolve(undefined)),
			notify: (message, type) => request("notify", type === undefined ? { message } : { message, type }),
			onTerminalInput(handler) {
				if (!shown) return () => {};
				if (terminalListeners.size === 0) request("setTerminalInput", { listening: true });
				terminalListeners.add(handler);
				return () => {
					if (terminalListeners.delete(handler) && terminalListeners.size === 0) request("setTerminalInput", { listening: false });
				};
			},
			setStatus: (key, text) => request("setStatus", { key, text }),
			setWorkingMessage: (message) => request("setWorkingMessage", { message }),
			setWorkingVisible: (visible) => request("setWorkingVisible", { visible }),
			setWorkingIndicator: (options) => request("setWorkingIndicator", { options: plain(options) }),
			setHiddenThinkingLabel: (label) => request("setHiddenThinkingLabel", { label }),
			setWidget(key, content, options) {
				const previous = widgetHandles.get(key);
				widgetHandles.delete(key);
				if (typeof content === "function") {
					// Modes without components show no component widgets.
					if (!shown) return;
					const handle = mount(content(tui, theme));
					widgetHandles.set(key, handle);
					request("setWidget", { key, handle, options: plain(options) });
				} else {
					request("setWidget", { key, lines: Array.isArray(content) ? content.map(String) : undefined, options: plain(options) });
				}
				if (previous !== undefined) unmount(previous);
			},
			setFooter: (factory) => replaceSlot("footer", factory, footerData),
			setHeader: (factory) => replaceSlot("header", factory),
			setTitle: (title) => request("setTitle", { title }),
			custom(factory, options) {
				if (!shown) return Promise.resolve(undefined);
				return new Promise((resolve, reject) => {
					let handle;
					let finished = false;
					const done = (result) => {
						if (finished) return;
						finished = true;
						if (handle !== undefined) {
							request("close", { handle });
							unmount(handle);
						}
						resolve(result);
					};
					keybindings()
						.then((manager) => factory(tui, theme, manager, done))
						.then((component) => {
							if (finished) {
								component?.dispose?.();
								return;
							}
							handle = mount(component);
							// pi-tui focuses the component it shows.
							if (component && "focused" in component) component.focused = true;
							// Without options an overlay takes the component's `width`, as in pi.
							const overlayOptions = options?.overlayOptions
								? typeof options.overlayOptions === "function"
									? options.overlayOptions()
									: options.overlayOptions
								: component?.width
									? { width: component.width }
									: undefined;
							request("custom", { handle, overlay: !!options?.overlay, overlayOptions: plain(overlayOptions) });
							options?.onHandle?.(overlayHandle(handle));
						})
						.catch(reject);
				});
			},
			pasteToEditor: (text) => request("pasteToEditor", { text }),
			setEditorText: (text) => request("setEditorText", { text }),
			getEditorText: () => request("getEditorText", {}) ?? "",
			addAutocompleteProvider(factory) {
				if (shown) addCompletions(factory);
			},
			setEditorComponent(factory) {
				if (shown) setEditor(factory);
			},
			getEditorComponent: () => (shown ? editorSlot.factory : undefined),
			theme,
			getAllThemes: () => request("getAllThemes", {}) ?? [],
			getTheme: () => undefined,
			setTheme: () => ({ success: false, error: "Themes cannot be changed from yapi extensions yet" }),
			getToolsExpanded: () => !!request("getToolsExpanded", {}),
			setToolsExpanded: (expanded) => request("setToolsExpanded", { expanded }),
		};
	}

	const sessionMethods = [
		"getCwd",
		"getSessionDir",
		"getSessionId",
		"getSessionFile",
		"getLeafId",
		"getLeafEntry",
		"getEntry",
		"getLabel",
		"getBranch",
		"getHeader",
		"getEntries",
		"getTree",
		"getSessionName",
		"getContext",
		"buildSessionContext",
		"isPersisted",
		"getChildren",
	];
	function createSessionManager() {
		const manager = {};
		for (const method of sessionMethods) manager[method] = (...args) => yapi.request("session.read", { method, args: plain(args) });
		return manager;
	}
	function createModelRegistry() {
		return {
			find: (provider, id) => yapi.request("models.find", { provider, id }) ?? undefined,
			getAll: () => yapi.request("models.all", {}),
			getAvailable: () => yapi.request("models.available", {}),
			getApiKey: (model) => yapi.op("models.apiKey", { provider: model.provider, id: model.id }),
			getApiKeyForProvider: (provider) => yapi.op("models.apiKey", { provider }),
			getProviderAuth: (provider) => yapi.op("models.auth", { provider }),
			isUsingOAuth: (model) => !!yapi.request("models.usingOAuth", { provider: model.provider }),
			hasConfiguredAuth: (model) => !!yapi.request("models.hasAuth", { provider: model.provider }),
			getError: () => yapi.request("models.error", {}) ?? undefined,
			getProviderDisplayName: (provider) => yapi.request("models.providerName", { provider }) ?? provider,
			getModelsOfType: (type, provider) => yapi.request("models.ofType", { type, provider }) ?? [],
			getModelOfType: (type, provider, id) => ofType(type, provider, id),
			findOfType: (type, provider, id) => ofType(type, provider, id),
			getAvailableOfType: (type, provider) => yapi.op("models.availableOfType", { type, provider }),
			// Only the provider and id cross: the host runs its own catalog entry with its credentials.
			classify: (model, context, options) =>
				yapi.op("models.classify", {
					provider: model?.provider,
					id: model?.id,
					context,
					temperature: options?.temperature,
				}),
			generateImages: (model, context) =>
				yapi.op("models.generateImages", { provider: model?.provider, id: model?.id, context }),
			refresh: async (options) => {
				const result = await yapi.op("models.refresh", {
					providers: options?.providers,
					allowNetwork: options?.allowNetwork,
					force: options?.force,
				});
				const errors = new Map(Object.entries(result?.errors ?? {}).map(([id, message]) => [id, new Error(message)]));
				return { aborted: !!result?.aborted, errors };
			},
		};
	}
	function ofType(type, provider, id) {
		return (yapi.request("models.ofType", { type, provider }) ?? []).find((model) => model.id === id);
	}
	function createContext(data = {}, extra = {}) {
		const controller = new AbortController();
		const context = {
			ui: createUi(data),
			mode: data.mode ?? "print",
			hasUI: !!data.hasUI,
			cwd: data.cwd ?? yapi.cwd,
			sessionManager: createSessionManager(),
			modelRegistry: createModelRegistry(),
			model: data.model ?? undefined,
			scopedModels: data.scopedModels ?? [],
			thinkingLevel: data.thinkingLevel,
			signal: data.aborted ? AbortSignal.abort() : controller.signal,
			isIdle: () => !!yapi.request("agent.isIdle", {}),
			isProjectTrusted: () => !!data.projectTrusted,
			abort: () => yapi.request("agent.abort", {}),
			hasPendingMessages: () => !!yapi.request("agent.hasPendingMessages", {}),
			shutdown: () => yapi.request("agent.shutdown", {}),
			getContextUsage: () => yapi.request("agent.contextUsage", {}) ?? undefined,
			compact: (options) => {
				yapi.op("agent.compact", { customInstructions: options?.customInstructions }).then(
					(result) => options?.onComplete?.(result),
					(error) => options?.onError?.(error),
				);
			},
			getSystemPrompt: () => yapi.request("agent.systemPrompt", {}) ?? "",
			...extra,
		};
		return context;
	}
	function createCommandContext(data) {
		return createContext(data, {
			getSystemPromptOptions: () => yapi.request("agent.systemPromptOptions", {}) ?? {},
			waitForIdle: () => yapi.op("agent.waitForIdle", {}),
			newSession: (options) => yapi.op("session.new", { parentSession: options?.parentSession }),
			fork: (entryId, options) => yapi.op("session.fork", { entryId, position: options?.position }),
			navigateTree: (targetId, options) => yapi.op("session.navigateTree", { targetId, ...plain(options) }),
			switchSession: (sessionPath) => yapi.op("session.switch", { sessionPath }),
			reload: () => yapi.op("session.reload", {}),
		});
	}

	// ----- bash operations -----------------------------------------------------------------------
	// Operations `user_bash` handlers returned, by id, until the host runs them.
	const bashOperations = new Map();
	let nextBashOperations = 1;
	const bashAborts = new Map();
	/** pi's `isUserBashEventResult`. */
	function isUserBashEventResult(value) {
		if (typeof value !== "object" || value === null) return false;
		const hasOperations = value.operations !== undefined;
		const hasResult = value.result !== undefined;
		if (hasOperations === hasResult) return false;
		if (hasOperations) {
			return typeof value.operations === "object" && value.operations !== null && typeof value.operations.exec === "function";
		}
		const result = value.result;
		if (typeof result !== "object" || result === null) return false;
		return (
			typeof result.output === "string" &&
			"exitCode" in result &&
			(result.exitCode === undefined || typeof result.exitCode === "number") &&
			typeof result.cancelled === "boolean" &&
			typeof result.truncated === "boolean" &&
			(result.fullOutputPath === undefined || typeof result.fullOutputPath === "string")
		);
	}

	// ----- emitting to one extension -----------------------------------------------------------
	/** Runs `extension`'s handlers for `event` as pi's runner does for each handler. */
	async function emit(extension, event, ctx) {
		const handlers = [...(extension.handlers.get(event.type) ?? [])];
		// What JSON cannot carry: the abort signal and pi's file operation sets.
		if (event.type === "session_before_compact" || event.type === "session_before_tree") event.signal = ctx.signal;
		const fileOps = event.type === "session_before_compact" ? event.preparation?.fileOps : undefined;
		if (fileOps) for (const key of ["read", "written", "edited"]) fileOps[key] = new Set(fileOps[key] ?? []);
		const errors = [];
		const guard = async (run) => {
			try {
				return await run();
			} catch (error) {
				errors.push(errorInfo(error));
				return undefined;
			}
		};
		let result;
		switch (event.type) {
			case "tool_call": {
				// Errors propagate: a failing guard blocks the call. Handlers
				// change the arguments by editing `event.input` in place.
				for (const handler of handlers) {
					const handlerResult = await handler(event, ctx);
					if (handlerResult) {
						result = handlerResult;
						if (result.block) break;
					}
				}
				result = { ...result, input: event.input };
				break;
			}
			case "tool_result": {
				const current = { ...event };
				let modified = false;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler(current, ctx));
					if (!handlerResult) continue;
					for (const key of ["content", "details", "structuredContent", "isError", "usage"]) {
						if (handlerResult[key] !== undefined) {
							current[key] = handlerResult[key];
							modified = true;
						}
					}
					if (handlerResult.content !== undefined && handlerResult.structuredContent === undefined) delete current.structuredContent;
				}
				if (modified) result = { content: current.content, details: current.details, structuredContent: current.structuredContent, isError: current.isError, usage: current.usage };
				break;
			}
			case "message_end": {
				let message = event.message;
				let modified = false;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler({ ...event, message }, ctx));
					if (!handlerResult?.message) continue;
					if (handlerResult.message.role !== message.role) {
						errors.push({ error: "message_end handlers must return a message with the same role" });
						continue;
					}
					message = handlerResult.message;
					modified = true;
				}
				if (modified) result = { message };
				break;
			}
			case "input": {
				let text = event.text;
				let images = event.images;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler({ ...event, text, images }, ctx));
					if (handlerResult?.action === "handled") {
						result = handlerResult;
						break;
					}
					if (handlerResult?.action === "transform") {
						text = handlerResult.text;
						images = handlerResult.images ?? images;
					}
				}
				result ??= text !== event.text || images !== event.images ? { action: "transform", text, images } : { action: "continue" };
				break;
			}
			case "before_agent_start": {
				const messages = [];
				let systemPrompt;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler(event, ctx));
					if (handlerResult?.message) messages.push(handlerResult.message);
					if (handlerResult?.systemPrompt !== undefined) systemPrompt = handlerResult.systemPrompt;
				}
				result = { messages, systemPrompt };
				break;
			}
			case "context":
			case "context_with_system": {
				// Handlers return a new list or edit `event.messages` in place.
				let messages = event.messages;
				let changed = false;
				for (const handler of handlers) {
					const visible = messages;
					const snapshot = visible.slice();
					const handlerResult = await guard(() => handler({ ...event, messages: visible }, ctx));
					const edited = visible.length !== snapshot.length || visible.some((message, index) => message !== snapshot[index]);
					const returned = handlerResult?.messages ?? (edited ? visible : undefined);
					if (!returned) continue;
					messages = returned;
					changed = true;
				}
				if (changed) result = { messages };
				break;
			}
			case "before_provider_headers": {
				// Handlers change `event.headers` in place.
				for (const handler of handlers) await guard(() => handler(event, ctx));
				result = { headers: event.headers };
				break;
			}
			case "turn_end":
			case "agent_before_settle": {
				// pi's `emitBoundary` for this extension's handlers: each sees the
				// entries and continuation the earlier ones staged, and the context
				// they would make, built when read.
				let entries = event.entries;
				let shouldContinue = event.continue;
				for (const handler of handlers) {
					const staged = entries;
					const boundaryEvent = { ...event, entries: staged, continue: shouldContinue };
					Object.defineProperty(boundaryEvent, "context", {
						enumerable: true,
						get: () => yapi.request("session.boundaryContext", { type: event.type, entries: plain(staged) }),
					});
					const handlerResult = await guard(() => handler(boundaryEvent, ctx));
					if (handlerResult?.entries !== undefined) entries = handlerResult.entries;
					if (handlerResult?.continue !== undefined) shouldContinue = handlerResult.continue;
				}
				result = { entries, continue: shouldContinue };
				break;
			}
			case "before_provider_request": {
				let payload = event.payload;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler({ ...event, payload }, ctx));
					if (handlerResult !== undefined) payload = handlerResult;
				}
				result = { payload };
				break;
			}
			case "user_bash": {
				// Errors propagate: a failing handler stops the command.
				for (const handler of handlers) {
					const handlerResult = await handler(event, ctx);
					if (handlerResult === undefined) continue;
					if (!isUserBashEventResult(handlerResult)) {
						throw new Error(
							"Invalid user_bash handler result: return undefined for local execution or exactly one valid { operations } or { result } object",
						);
					}
					if (handlerResult.operations) {
						const id = nextBashOperations++;
						bashOperations.set(id, handlerResult.operations);
						result = { operations: { id } };
					} else {
						result = { result: handlerResult.result };
					}
					break;
				}
				break;
			}
			case "resources_discover": {
				const collected = { skillPaths: [], promptPaths: [], themePaths: [] };
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler(event, ctx));
					for (const key of Object.keys(collected)) if (handlerResult?.[key]?.length) collected[key].push(...handlerResult[key]);
				}
				result = collected;
				break;
			}
			case "cache_warming_decision": {
				let actionValue = event.action;
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler(event, ctx));
					if (handlerResult?.action !== undefined) actionValue = handlerResult.action;
				}
				result = { action: actionValue };
				break;
			}
			default: {
				const cancellable = event.type.startsWith("session_before_");
				for (const handler of handlers) {
					const handlerResult = await guard(() => handler(event, ctx));
					if (cancellable && handlerResult) {
						result = handlerResult;
						if (result.cancel) break;
					}
				}
			}
		}
		return { result: plain(result), errors };
	}

	// ----- provider streams --------------------------------------------------------------------
	/** Aborts of running streams and sign-ins, by the host's id. */
	const aborts = new Map();
	/**
	 * An event for the host: `start` with its message, `done` and `error` with
	 * the final one, the others without the live message but with its usage.
	 */
	function wireEvent(event) {
		switch (event.type) {
			case "start":
				return { type: "start", message: plain(event.partial) };
			case "done":
				return { type: "done", message: plain(event.message) };
			case "error":
				return { type: "error", message: plain(event.error) };
			default: {
				const { partial, ...rest } = event;
				if (event.type === "toolcall_start") {
					const block = partial?.content?.[event.contentIndex];
					rest.id = block?.id;
					rest.toolName = block?.name;
				}
				return { type: "update", event: plain(rest), usage: plain(partial?.usage) };
			}
		}
	}
	/** Runs `run` with an abort signal that the host's `abort` of `id` fires. */
	async function abortable(id, run) {
		const controller = new AbortController();
		aborts.set(id, controller);
		try {
			return await run(controller.signal);
		} finally {
			aborts.delete(id);
		}
	}
	/** Runs `run` with provider `payload.provider`'s `oauth` as operation `payload.id`. */
	function withOAuth(payload, run) {
		const oauth = providerConfig(payload.provider)?.oauth;
		if (!oauth) throw new Error(`Provider ${payload.provider} has no OAuth sign-in`);
		return abortable(payload.id, async (signal) => plain(await run(oauth, signal)) ?? null);
	}
	/** pi's `lazyStream` message for a stream that failed; pi-ai's facade uses it too. */
	const setupError = (yapi.setupError = (model, error) => ({
		role: "assistant",
		content: [],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
		stopReason: "error",
		errorMessage: errorMessage(error),
		timestamp: Date.now(),
	}));

	// ----- dispatch -------------------------------------------------------------------------------
	const extensionOf = (id) => {
		const extension = extensions.get(id);
		if (!extension) throw new Error(`Unknown extension ${id}`);
		return extension;
	};
	const kinds = {
		async load(payload) {
			yapi.cwd = payload.cwd;
			for (const [name, value] of Object.entries(payload.flags ?? {})) flagValues.set(name, value);
			const results = [];
			for (const entry of payload.extensions) results.push(await loadOne(entry));
			// Wire APIs registered with pi-ai's `registerApiProvider`.
			return { extensions: results, apis: [...(yapi.apiProviders?.keys() ?? [])] };
		},
		async bind() {
			bound = true;
			let spec = null;
			let keys = null;
			try {
				spec = yapi.request("ui.theme", {}) ?? null;
				keys = yapi.request("ui.keybindings", {}) ?? null;
			} catch {
				// Hosts without a UI leave text plain.
			}
			theme.load(spec);
			// Components match keys as the host's bindings do.
			if (keys) {
				tuiModule ??= await import("@earendil-works/pi-tui");
				facade ??= await import("@earendil-works/pi-coding-agent");
				tuiModule.setKittyProtocolActive(!!keys.kitty);
				const definitions = Object.fromEntries(Object.entries(keys.bindings ?? {}).map(([id, defaultKeys]) => [id, { defaultKeys }]));
				tuiModule.setKeybindings(new tuiModule.KeybindingsManager(definitions));
				editorActions = keys.actions ?? [];
			}
			return null;
		},
		async reload() {
			bound = false;
			for (const handle of [...components.keys()]) unmount(handle);
			widgetHandles.clear();
			transcriptViews.clear();
			slots.footer = slots.header = undefined;
			editorSlot.factory = editorSlot.handle = undefined;
			terminalListeners.clear();
			completionWrappers.length = 0;
			completions = hostCompletions;
			const results = [];
			for (const [id, extension] of [...extensions]) {
				extensions.delete(id);
				results.push(await instantiate(id, extension.path, extension.factory));
			}
			return { extensions: results };
		},
		flags(payload) {
			for (const [name, value] of Object.entries(payload.values ?? {})) flagValues.set(name, value);
			return null;
		},
		emit(payload) {
			return emit(extensionOf(payload.extension), payload.event, createContext(payload.ctx));
		},
		async tool(payload) {
			const extension = extensionOf(payload.extension);
			const tool = extension.tools.get(payload.name);
			if (!tool) throw new Error(`Tool ${payload.name} is not registered by ${extension.path}`);
			const ctx = createContext(payload.ctx, {
				tools: payload.tools ?? [],
				executeTool: (name, args) => yapi.op("tool.execute", { toolCallId: payload.toolCallId, name, args: plain(args) }),
			});
			const onUpdate = (partial) => yapi.request("tool.update", { toolCallId: payload.toolCallId, partial: plain(partial) });
			let params = payload.params;
			if (typeof tool.prepareArguments === "function") params = tool.prepareArguments(params);
			const result = await tool.execute(payload.toolCallId, params, ctx.signal, onUpdate, ctx);
			return plain(result) ?? { content: [] };
		},
		async command(payload) {
			const command = extensionOf(payload.extension).commands.get(payload.name);
			if (!command) throw new Error(`Command /${payload.name} is not registered`);
			await command.handler(payload.args ?? "", createCommandContext(payload.ctx));
			return null;
		},
		async complete(payload) {
			const command = extensionOf(payload.extension).commands.get(payload.name);
			if (typeof command?.getArgumentCompletions !== "function") return null;
			return plain(await command.getArgumentCompletions(payload.prefix ?? "")) ?? null;
		},
		/**
		 * The composed providers' suggestions for the host's editor, with what
		 * applying each item gives, since the editor applies one at once.
		 */
		async autocomplete(payload) {
			const { lines, cursorLine, cursorCol, force } = payload;
			const provider = completions;
			const suggestions = await provider.getSuggestions(lines, cursorLine, cursorCol, { signal: new AbortController().signal, force });
			if (!Array.isArray(suggestions?.items) || suggestions.items.length === 0) return null;
			const applied = suggestions.items.map((item) => {
				try {
					return plain(provider.applyCompletion(lines, cursorLine, cursorCol, item, suggestions.prefix)) ?? null;
				} catch (error) {
					console.error("Autocomplete error:", error);
					return null;
				}
			});
			return { prefix: suggestions.prefix ?? "", items: plain(suggestions.items), applied };
		},
		/** Runs the input listeners over each key as pi-tui does; `null` for a consumed key. */
		terminalInput(payload) {
			return (payload.keys ?? []).map((data) => {
				let current = data;
				for (const listener of terminalListeners) {
					let result;
					try {
						result = listener(current);
					} catch (error) {
						console.error("Terminal input listener error:", error);
						continue;
					}
					if (result?.consume) return null;
					if (result?.data !== undefined) current = result.data;
				}
				return current;
			});
		},
		/** An operation the host sends its editor; see `ComponentHost::editor_op`. */
		editor(payload) {
			const editor = components.get(payload.handle);
			if (payload.op === "setText") editor?.setText?.(payload.text ?? "");
			else if (payload.op === "addToHistory") editor?.addToHistory?.(payload.text ?? "");
			else if (payload.op === "configure" && editor) {
				editor.borderColor = payload.border === "bashMode" ? theme.getBashModeBorderColor() : theme.getThinkingBorderColor(payload.border);
				editor.setPaddingX?.(payload.paddingX);
				editor.setAutocompleteMaxVisible?.(payload.autocompleteMaxVisible);
				if ("focused" in editor) editor.focused = !!payload.focused;
				tui.terminal.rows = payload.rows;
			}
			return null;
		},
		/** Runs a `!` command through the operations with `id`; `{ exitCode }`. */
		async bash(payload) {
			const operations = bashOperations.get(payload.id);
			bashOperations.delete(payload.id);
			if (!operations) throw new Error(`No bash operations ${payload.id}`);
			const controller = new AbortController();
			bashAborts.set(payload.id, controller);
			try {
				const onData = (data) => yapi.request("bash.data", { id: payload.id, data: yapi.base64Encode(Buffer.from(data)) });
				const result = await operations.exec(payload.command, payload.cwd, { onData, signal: controller.signal });
				return { exitCode: result?.exitCode ?? null };
			} finally {
				bashAborts.delete(payload.id);
			}
		},
		bashAbort(payload) {
			bashAborts.get(payload.id)?.abort();
			return null;
		},
		async shortcut(payload) {
			const shortcut = extensionOf(payload.extension).shortcuts.get(payload.shortcut);
			if (!shortcut) throw new Error(`Shortcut ${payload.shortcut} is not registered`);
			await shortcut.handler(createContext(payload.ctx));
			return null;
		},
		/**
		 * Streams a response for a model whose API an extension implements:
		 * its provider's `streamSimple` when the API matches, else the API's
		 * `registerApiProvider` stream, as pi's provider composer picks. Each
		 * event goes to the host as it arrives. A stream that throws ends
		 * with pi's setup error.
		 */
		async stream(payload) {
			const { id, model, context } = payload;
			const send = (event) => yapi.request("provider.event", { id, ...event });
			await abortable(id, async (signal) => {
				try {
					const config = providerConfig(model.provider);
					const options = { ...payload.options, signal };
					let events;
					if (typeof config?.streamSimple === "function" && config.api === model.api) events = config.streamSimple(model, context, options);
					else {
						const api = yapi.apiProviders?.get(model.api)?.provider;
						if (!api) throw new Error(`No API provider registered for api: ${model.api}`);
						events = api.streamSimple(model, context, options);
					}
					for await (const event of events) {
						send(wireEvent(event));
						if (event.type === "done" || event.type === "error") break;
					}
				} catch (error) {
					send({ type: "error", message: setupError(model, error) });
				}
			});
			return null;
		},
		/** Aborts stream or sign-in `id`. */
		abort(payload) {
			aborts.get(payload.id)?.abort();
			return null;
		},
		/**
		 * Runs provider `provider`'s `oauth.login` with callbacks that reach
		 * the host's sign-in `id`, as pi's provider composer adapts them.
		 */
		oauthLogin(payload) {
			const notify = (event) => yapi.request("oauth.notify", { id: payload.id, event });
			const prompt = (question) => yapi.op("oauth.prompt", { id: payload.id, prompt: question });
			return withOAuth(payload, (oauth, signal) =>
				oauth.login({
					onAuth: (info) => notify({ type: "auth_url", ...info }),
					onDeviceCode: (info) => notify({ type: "device_code", ...info }),
					onPrompt: (question) => prompt({ type: "text", ...question }),
					onProgress: (message) => notify({ type: "progress", message }),
					onManualCodeInput: () => prompt({ type: "manual_code", message: "Paste the authorization code" }),
					onSelect: (question) => prompt({ type: "select", ...question }),
					signal,
				}),
			);
		},
		oauthRefresh(payload) {
			return withOAuth(payload, (oauth, signal) => oauth.refreshToken(payload.credential, signal));
		},
		oauthApiKey(payload) {
			return withOAuth(payload, (oauth) => oauth.getApiKey(payload.credential));
		},
		/**
		 * Runs one phase of provider `provider`'s `refreshModels`: `{ models }`,
		 * `null` when it returns none, with the last entry it asked `publish`
		 * to persist (`null` to delete) as `persist`. The host writes it.
		 */
		async refreshModels(payload) {
			const config = providerConfig(payload.provider);
			if (typeof config?.refreshModels !== "function") return { models: null };
			const result = {};
			const models = await abortable(payload.id, (signal) =>
				config.refreshModels({
					credential: payload.credential ?? undefined,
					stored: payload.stored ?? undefined,
					allowNetwork: !!payload.allowNetwork,
					force: payload.allowNetwork ? payload.force : undefined,
					signal,
					publish: async (publication) => {
						if (publication.persist !== undefined) result.persist = plain(publication.persist) ?? null;
						publication.update?.();
						return true;
					},
				}),
			);
			result.models = plain(models) ?? null;
			return result;
		},
		/**
		 * Builds the component a tool's `renderCall`/`renderResult` or a
		 * message renderer returns; `{ handle }`, or `null` for the built-in
		 * rendering. A renderer that returns its last component keeps its
		 * handle.
		 */
		component(payload) {
			const extension = extensionOf(payload.extension);
			const key = payload.kind === "message" ? `message:${payload.key}` : `tool:${payload.toolCallId}`;
			const views = transcriptViews.get(key) ?? { state: {}, call: undefined, result: undefined, message: undefined };
			transcriptViews.set(key, views);
			const slot = payload.kind === "toolCall" ? "call" : payload.kind === "toolResult" ? "result" : "message";
			const previous = views[slot];
			let component;
			try {
				if (slot === "message") {
					const renderer = extension.messageRenderers.get(payload.message?.customType);
					component = renderer?.(payload.message, plain(payload.options ?? {}), theme);
				} else {
					const tool = extension.tools.get(payload.name);
					const context = {
						...(payload.context ?? {}),
						args: payload.args,
						toolCallId: payload.toolCallId,
						invalidate: () => tui.requestRender(),
						lastComponent: previous?.component,
						state: views.state,
					};
					component =
						slot === "call"
							? tool?.renderCall?.(payload.args, theme, context)
							: tool?.renderResult?.(payload.result, plain(payload.options ?? {}), theme, context);
				}
			} catch {
				// pi falls back to the default rendering without a word.
				component = undefined;
			}
			if (!component) {
				if (previous) unmount(previous.handle);
				views[slot] = undefined;
				return null;
			}
			if (previous?.component === component) return { handle: previous.handle };
			if (previous) unmount(previous.handle);
			const handle = mount(component);
			views[slot] = { handle, component };
			return { handle };
		},
	};

	yapi.dispatch = (id, kind, payloadText) => {
		const handler = kinds[kind];
		Promise.resolve()
			.then(() => {
				if (!handler) throw new Error(`Unknown dispatch kind: ${kind}`);
				return handler(JSON.parse(payloadText));
			})
			.then(
				(value) => native.done(id, JSON.stringify(value ?? null) ?? "null"),
				(error) => native.fail(id, errorMessage(error)),
			);
	};
	yapi.extensions = extensions;
})();
