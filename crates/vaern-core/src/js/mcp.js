// JavaScript half of vaern:mcp, evaluated once at startup, after vaern:ai.
// The native side owns every pipe, the framing, version negotiation and
// each server's process; this file checks a server declaration and runs the
// tools it names, and shapes connections and tool handles for a client.
(() => {
    const { ToolError, BudgetError } = globalThis.__vaern_ai;

    class McpError extends Error {
        constructor(info) {
            super(info.message);
            this.name = "McpError";
            this.stage = info.stage;
            this.code = info.code;
            this.server = info.server;
            this.action = info.action;
            if (info.tool !== undefined) this.tool = info.tool;
            if (info.exit_code !== undefined) this.exitCode = info.exit_code;
            if (info.stderr !== undefined) this.stderr = info.stderr;
        }
    }

    const TOOL_NAME = /^[a-zA-Z0-9_-]{1,64}$/;
    let declared = null;

    // server({ name, version, tools, instructions }): the tools are tool()
    // values, so one definition serves an agent() in this process and MCP
    // clients of `vaern mcp serve`.
    function server(options) {
        if (options === null || typeof options !== "object") {
            throw new TypeError("server: options must be an object");
        }
        const { name, version, instructions } = options;
        if (typeof name !== "string" || name === "") throw new TypeError("server: name must be a string");
        if (typeof version !== "string" || version === "") {
            throw new TypeError("server: version must be a string");
        }
        if (instructions !== undefined && typeof instructions !== "string") {
            throw new TypeError("server: instructions must be a string");
        }
        const tools = options.tools;
        if (!Array.isArray(tools)) throw new TypeError("server: tools must be a list");
        const byName = new Map();
        for (const t of tools) {
            if (!t || typeof t.call !== "function" || typeof t.name !== "string" || !TOOL_NAME.test(t.name)) {
                throw new TypeError("server: each tool must be a value returned by tool()");
            }
            if (byName.has(t.name)) throw new TypeError(`server: two tools are named ${t.name}`);
            byName.set(t.name, t);
        }
        const declaration = JSON.stringify({
            name, version, instructions,
            tools: tools.map((t) => ({ name: t.name, description: t.description, inputSchema: t.parameters })),
        });
        const refused = __vaern_mcp_declare(declaration);
        if (refused) throw new McpError(JSON.parse(refused));
        declared = byName;
    }

    // Runs one tools/call and hands the outcome back. Bad arguments are the
    // client's mistake and go back as a protocol error; any other failure
    // is the tool's and goes back as a result with isError set.
    globalThis.__vaern_mcp_call = (reqId, name, argsJson) => {
        const reply = (outcome) => __vaern_mcp_reply(reqId, JSON.stringify(outcome));
        const t = declared && declared.get(name);
        if (!t) {
            reply({ ok: false, kind: "args", message: `Unknown tool: ${name}` });
            return;
        }
        Promise.resolve()
            .then(() => t.call(JSON.parse(argsJson)))
            .then(
                (value) => reply({ ok: true, value }),
                (err) => {
                    const kind = err && err.name === "ToolError" && err.stage === "args" ? "args" : "tool";
                    const message = err instanceof Error ? err.message : String(err);
                    reply({ ok: false, kind, message });
                },
            );
    };

    // Client side. Connects, calls and closes share one request id space;
    // a connection is named by the id of the connect() that opened it.
    const pending = new Map();
    let nextReqId = 1;
    const failure = (info) => (info.budget ? new BudgetError(info) : new McpError(info));

    // start(reqId) returns null once the native side has the request, or
    // the JSON of what stopped it; { done: true } means nothing was needed.
    function request(start) {
        const reqId = nextReqId++;
        return new Promise((resolve, reject) => {
            pending.set(reqId, { resolve, reject });
            let refused;
            try {
                refused = start(reqId);
            } catch (err) {
                pending.delete(reqId);
                reject(err);
                return;
            }
            if (!refused) return;
            pending.delete(reqId);
            const info = JSON.parse(refused);
            if (info.done) resolve(info);
            else reject(failure(info));
        });
    }

    globalThis.__vaern_dispatch_mcp = (json) => {
        const info = JSON.parse(json);
        const waiter = pending.get(info.reqId);
        if (!waiter) return;
        // Built before the waiter is removed, so a throw here leaves it for
        // __vaern_reject_mcp.
        const error = info.ok ? null : failure(info);
        pending.delete(info.reqId);
        if (info.ok) waiter.resolve(info);
        else waiter.reject(error);
    };

    globalThis.__vaern_reject_mcp = (reqId, err) => {
        const waiter = pending.get(reqId);
        if (!waiter) return false;
        pending.delete(reqId);
        waiter.reject(err);
        return true;
    };

    // What an aborted call rejects with, built as fetch builds it.
    const abortReason = (signal) => {
        const err = signal.reason || new Error("The operation was aborted");
        err.name = "AbortError";
        return err;
    };

    const usdOf = (where, value) => {
        if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
            throw new TypeError(`${where} must be a number, 0 or more`);
        }
        return Math.round(value * 1e9);
    };

    // What a call resolves to: the structured result when there is one,
    // else the text of its text blocks, else the content list as sent.
    function valueOf(result) {
        if (result.structuredContent !== undefined && result.structuredContent !== null) {
            return result.structuredContent;
        }
        const content = Array.isArray(result.content) ? result.content : [];
        const texts = content.filter((block) => block && block.type === "text");
        return texts.length > 0 ? texts.map((block) => block.text).join("") : content;
    }

    function handle(connId, serverName, definition, costUsd) {
        const name = definition.name;
        const costNano = Math.round(costUsd * 1e9);
        // A refusal of the arguments goes back to an agent's model to
        // correct, as a local tool's would; a failed run is the tool's. An
        // aborted signal rejects the call at once, as fetch does; the server
        // is told to stop it, and the call is recorded and charged as sent.
        async function call(args, options) {
            const signal = options?.signal ?? undefined;
            if (signal !== undefined && !(signal instanceof AbortSignal)) {
                throw new TypeError(`${name}.call: signal must be an AbortSignal`);
            }
            if (signal?.aborted) throw abortReason(signal);
            let info;
            try {
                info = await request((reqId) => {
                    const refused = __vaern_mcp_call_tool(reqId, connId, name,
                        JSON.stringify(args ?? {}), costNano, signal ? signal._id : 0);
                    if (!refused && signal) {
                        signal.addEventListener("abort", () => {
                            const waiter = pending.get(reqId);
                            if (!waiter) return;
                            pending.delete(reqId);
                            __vaern_mcp_cancel(reqId);
                            waiter.reject(abortReason(signal));
                        });
                    }
                    return refused;
                });
            } catch (err) {
                if (err instanceof McpError && err.code === "REFUSED") {
                    throw new ToolError(name, "args", err.message, { server: serverName });
                }
                throw err;
            }
            const result = info.result;
            if (result.isError) {
                const text = valueOf({ content: result.content });
                const message = typeof text === "string" && text !== "" ? text : "the tool reported an error";
                throw new ToolError(name, "run", message, { server: serverName });
            }
            return valueOf(result);
        }
        return Object.freeze({
            name,
            description: typeof definition.description === "string" ? definition.description : "",
            parameters: definition.inputSchema ?? { type: "object", properties: {} },
            cost: Object.freeze({ usd: costUsd }),
            server: serverName,
            call,
        });
    }

    // connect({ script, budget?, timeoutMs?, cost? }) starts `vaern mcp
    // serve script`, agrees a revision and resolves to the connection.
    // budget is what the server may spend, reserved here and given to it as
    // its caps; timeoutMs is how long each call may wait, default 60000;
    // cost is what each tool's call is charged here, by tool name. A server
    // that exits on its own is started again by the next call, at most 3
    // times; restarts counts them.
    function connect(options) {
        if (options === null || typeof options !== "object") {
            return Promise.reject(new TypeError("connect: options must be an object"));
        }
        const { script } = options;
        let grantNano;
        let grantTokens;
        let costs;
        let timeoutMs;
        try {
            if (typeof script !== "string" || script === "") {
                throw new TypeError("connect: script must be the path of a server script");
            }
            const budget = options.budget ?? {};
            grantNano = usdOf("connect: budget.usd", budget.usd ?? 0);
            grantTokens = budget.tokens ?? 0;
            if (!Number.isSafeInteger(grantTokens) || grantTokens < 0) {
                throw new TypeError("connect: budget.tokens must be a whole number, 0 or more");
            }
            timeoutMs = options.timeoutMs ?? 60000;
            if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 2 ** 31) {
                throw new TypeError("connect: timeoutMs must be a whole number of milliseconds, 1 or more");
            }
            costs = new Map();
            for (const [tool, cost] of Object.entries(options.cost ?? {})) {
                usdOf(`connect: cost.${tool}.usd`, cost?.usd);
                costs.set(tool, cost.usd);
            }
        } catch (err) {
            return Promise.reject(err);
        }
        return request((reqId) => __vaern_mcp_connect(reqId, script, grantNano, grantTokens, timeoutMs))
            .then((info) => {
                const connId = info.connId;
                const serverName = info.server?.name ?? script;
                const tools = Object.freeze((info.tools ?? []).map((definition) =>
                    handle(connId, serverName, definition, costs.get(definition.name) ?? 0)));
                let closing = null;
                return Object.freeze({
                    name: serverName,
                    version: info.server?.version,
                    protocolVersion: info.protocolVersion,
                    instructions: info.instructions ?? undefined,
                    get restarts() {
                        return __vaern_mcp_restarts(connId);
                    },
                    tools: () => Promise.resolve(tools),
                    close: () => {
                        closing ??= request((reqId) => __vaern_mcp_close(reqId, connId)).then(() => undefined);
                        return closing;
                    },
                });
            });
    }

    globalThis.__vaern_mcp = Object.freeze({ server, connect, McpError });
})();
