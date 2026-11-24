// JavaScript half of vaern:mcp, evaluated once at startup. The native side
// owns stdin, stdout, framing and version negotiation; this file checks a
// server declaration and runs the tools it names.
(() => {
    class McpError extends Error {
        constructor(info) {
            super(info.message);
            this.name = "McpError";
            this.stage = info.stage;
            this.code = info.code;
            this.server = info.server;
            this.action = info.action;
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

    globalThis.__vaern_mcp = Object.freeze({ server, McpError });
})();
