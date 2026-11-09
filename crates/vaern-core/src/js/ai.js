// JavaScript half of vaern:ai, evaluated once at startup. The native bridge
// (__vaern_ai_start) owns the request, the output cap, the charge and the
// journal record; this file only shapes options and results.
(() => {
    const pending = new Map();
    let nextReqId = 1;

    class ModelError extends Error {
        constructor(info) {
            super(info.message);
            this.name = "ModelError";
            this.stage = info.stage;
            this.status = info.status || undefined;
            this.url = info.url || undefined;
            this.action = info.action;
            if (info.path !== undefined) this.path = info.path;
            // A reply that failed its schema was still billed.
            if (info.chargedNano !== undefined) {
                this.costUsd = info.chargedNano / 1e9;
                this.effect = info.eid;
            }
        }
    }

    class ToolError extends Error {
        constructor(tool, stage, message, extra) {
            super(message);
            this.name = "ToolError";
            this.tool = tool;
            this.stage = stage;
            if (extra) Object.assign(this, extra);
        }
    }

    // reserve and remaining are in USD for unit "usd", tokens otherwise.
    class BudgetError extends Error {
        constructor(info) {
            super(info.message);
            this.name = "BudgetError";
            this.code = info.code;
            this.action = info.action;
            if (info.unit) {
                const scale = info.unit === "usd" ? 1e9 : 1;
                this.unit = info.unit;
                this.reserve = info.reserve / scale;
                this.remaining = info.remaining / scale;
            }
        }
    }

    const failure = (info) => (info.budget ? new BudgetError(info) : new ModelError(info));

    class AgentError extends Error {
        constructor(code, message, steps) {
            super(message);
            this.name = "AgentError";
            this.code = code;
            this.steps = steps;
        }
    }

    // The native side hands results over as JSON text: the reply carries the
    // provider's tool calls as nested JSON, and one parse here is simpler
    // than building nested objects through the bridge.
    globalThis.__vaern_dispatch_model = (json) => {
        const info = JSON.parse(json);
        const waiter = pending.get(info.reqId);
        if (!waiter) return;
        pending.delete(info.reqId);
        if (info.ok) waiter.resolve(info);
        else waiter.reject(failure(info));
    };

    globalThis.__vaern_model_delta = (reqId, delta) => {
        const waiter = pending.get(reqId);
        if (waiter && waiter.onDelta) waiter.onDelta(delta);
    };

    const fromEnv = (name) => {
        const value = globalThis.process && process.env ? process.env[name] : undefined;
        return value === "" ? undefined : value;
    };

    const objectArg = (where, value) => {
        if (value === null || typeof value !== "object") {
            throw new TypeError(`${where}: options must be an object`);
        }
        return value;
    };

    function start(config, options, onDelta) {
        const reqId = nextReqId++;
        const call = JSON.stringify({
            messages: options.messages,
            maxTokens: options.maxTokens,
            temperature: options.temperature,
            responseFormat: options.responseFormat,
            schema: options.schema,
            schemaName: options.schemaName,
            tools: options.tools,
            stream: onDelta !== undefined,
        });
        return new Promise((resolve, reject) => {
            pending.set(reqId, { resolve, reject, onDelta });
            let refused;
            try {
                refused = __vaern_ai_start(reqId, config, call);
            } catch (err) {
                pending.delete(reqId);
                reject(err);
                return;
            }
            if (refused) {
                pending.delete(reqId);
                reject(failure(JSON.parse(refused)));
            }
        });
    }

    const summary = (r) => ({
        text: r.text ?? "",
        usage: { inputTokens: r.inputTokens, outputTokens: r.outputTokens },
        costUsd: r.chargedNano / 1e9,
        finishReason: r.finishReason ?? null,
        effect: r.eid,
    });

    // An async iterator over { delta } events, then one final event with
    // done: true and the summary chat() returns. The request starts on the
    // first next(), so a stream that is never read sends nothing. Leaving
    // the loop early stops delivery, not the request: it still completes
    // and is recorded and charged.
    function streamOf(config, options) {
        const events = [];
        let wakers = [];
        let started = false;
        let ended = false;
        let closed = false;
        let failure = null;
        const wake = () => {
            const woken = wakers;
            wakers = [];
            for (const resolve of woken) resolve();
        };
        const begin = () => {
            started = true;
            start(config, options, (delta) => {
                if (!closed) events.push({ delta });
                wake();
            }).then(
                (r) => {
                    if (!closed) events.push({ delta: "", done: true, ...summary(r) });
                    ended = true;
                    wake();
                },
                (err) => {
                    failure = err;
                    ended = true;
                    wake();
                },
            );
        };
        return {
            [Symbol.asyncIterator]() {
                return this;
            },
            async next() {
                if (!started) begin();
                while (events.length === 0 && !ended && !closed) {
                    await new Promise((resolve) => wakers.push(resolve));
                }
                if (events.length > 0) return { value: events.shift(), done: false };
                if (failure && !closed) {
                    closed = true;
                    throw failure;
                }
                return { value: undefined, done: true };
            },
            async return() {
                closed = true;
                events.length = 0;
                wake();
                return { value: undefined, done: true };
            },
        };
    }

    function model(options) {
        objectArg("model", options);
        const price = options.price || {};
        const config = JSON.stringify({
            baseUrl: options.baseUrl ?? fromEnv("VAERN_AI_BASE_URL"),
            model: options.model,
            apiKey: options.apiKey ?? fromEnv("VAERN_AI_API_KEY"),
            inputPerMTok: price.inputPerMTok ?? 0,
            outputPerMTok: price.outputPerMTok ?? 0,
            maxOutputTokens: options.maxOutputTokens,
            timeoutMs: options.timeoutMs,
        });
        return Object.freeze({
            async chat(callOptions) {
                const r = await start(config, objectArg("chat", callOptions));
                const result = summary(r);
                if (r.json !== null) result.json = r.json;
                if (Array.isArray(r.toolCalls) && r.toolCalls.length > 0) {
                    result.toolCalls = r.toolCalls;
                }
                return result;
            },
            stream(callOptions) {
                return streamOf(config, objectArg("stream", callOptions));
            },
        });
    }

    const TOOL_NAME = /^[a-zA-Z0-9_-]{1,64}$/;
    let nextToolCall = 1;
    const parsed = (json) => (json ? JSON.parse(json) : null);

    function tool(options) {
        objectArg("tool", options);
        const { name, run } = options;
        const description = options.description ?? "";
        if (typeof name !== "string" || !TOOL_NAME.test(name)) {
            throw new TypeError("tool: name must be 1 to 64 letters, digits, '_' or '-'");
        }
        if (typeof run !== "function") throw new TypeError(`tool ${name}: run must be a function`);
        if (typeof description !== "string") {
            throw new TypeError(`tool ${name}: description must be a string`);
        }
        const parameters = options.parameters ?? { type: "object", properties: {} };
        const paramsJson = JSON.stringify(parameters);
        const problem = parsed(__vaern_schema_check(paramsJson));
        if (problem) {
            throw new TypeError(`tool ${name}: parameters at ${problem.path}: ${problem.message}`);
        }
        if (parameters.type !== "object") {
            throw new TypeError(`tool ${name}: parameters must describe an object`);
        }
        const costUsd = options.cost?.usd ?? 0;
        if (typeof costUsd !== "number" || !Number.isFinite(costUsd) || costUsd < 0) {
            throw new TypeError(`tool ${name}: cost.usd must be a number, 0 or more`);
        }

        // Arguments and results cross as JSON copies, so run() sees what the
        // journal records and the caller gets what a replay would return.
        async function call(args) {
            const argsJson = JSON.stringify(args ?? {});
            const mismatch = parsed(__vaern_schema_validate(argsJson, paramsJson));
            if (mismatch) {
                throw new ToolError(name, "args",
                    `arguments at ${mismatch.path}: ${mismatch.message}`, { path: mismatch.path });
            }
            const reqId = nextToolCall++;
            const started = JSON.parse(
                __vaern_tool_start(reqId, name, argsJson, Math.round(costUsd * 1e9)));
            if (started.budget) throw new BudgetError(started.budget);
            if (started.error) throw new ToolError(name, "journal", started.error);
            const finish = (ok, outcomeJson) => {
                const failed = parsed(__vaern_tool_finish(reqId, ok, outcomeJson));
                if (failed) throw new ToolError(name, "journal", failed.error, { effect: started.eid });
            };
            let value;
            try {
                value = await run(JSON.parse(argsJson));
            } catch (err) {
                const message = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
                finish(false, JSON.stringify({ stage: "run", message }));
                throw new ToolError(name, "run", message, { cause: err, effect: started.eid });
            }
            let resultJson;
            try {
                resultJson = JSON.stringify(value);
            } catch {
                resultJson = undefined;
            }
            if (resultJson === undefined) {
                const message = "the result is not JSON-serialisable";
                finish(false, JSON.stringify({ stage: "result", message }));
                throw new ToolError(name, "result", message, { effect: started.eid });
            }
            finish(true, resultJson);
            return JSON.parse(resultJson);
        }

        return Object.freeze({
            name,
            description,
            parameters,
            cost: Object.freeze({ usd: costUsd }),
            call,
        });
    }

    // The agent loop: a reply without tool calls is the answer; otherwise
    // each call runs in order and its result goes back as a tool message.
    // maxSteps bounds the number of model calls.
    function agent(options) {
        objectArg("agent", options);
        const m = options.model;
        if (!m || typeof m.chat !== "function") {
            throw new TypeError("agent: model must be a value returned by model()");
        }
        const system = options.system;
        if (system !== undefined && typeof system !== "string") {
            throw new TypeError("agent: system must be a string");
        }
        const tools = options.tools ?? [];
        if (!Array.isArray(tools)) throw new TypeError("agent: tools must be a list");
        const byName = new Map();
        for (const t of tools) {
            if (!t || typeof t.call !== "function" || typeof t.name !== "string") {
                throw new TypeError("agent: each tool must be a value returned by tool()");
            }
            if (byName.has(t.name)) throw new TypeError(`agent: two tools are named ${t.name}`);
            byName.set(t.name, t);
        }
        const maxSteps = options.maxSteps ?? 8;
        if (!Number.isInteger(maxSteps) || maxSteps < 1) {
            throw new TypeError("agent: maxSteps must be a whole number above 0");
        }
        const definitions = tools.map((t) => ({
            type: "function",
            function: { name: t.name, description: t.description, parameters: t.parameters },
        }));

        async function run(input) {
            const messages = [];
            if (system) messages.push({ role: "system", content: system });
            if (typeof input === "string") messages.push({ role: "user", content: input });
            else if (Array.isArray(input)) messages.push(...input);
            else throw new TypeError("agent.run: input must be a string or a list of messages");
            const steps = [];
            const usage = { inputTokens: 0, outputTokens: 0 };
            let costUsd = 0;
            let corrected = false;

            // An unknown tool or bad arguments go back to the model once to
            // correct; a second time ends the run with that ToolError.
            const refuse = (refusal) => {
                if (corrected) throw refusal;
                corrected = true;
                steps.push({ type: "tool", name: refusal.tool, error: refusal.message });
                return `Error: ${refusal.message}`;
            };
            const invoke = async (call) => {
                const name = call?.function?.name;
                const t = byName.get(name);
                if (!t) {
                    return refuse(new ToolError(String(name), "args",
                        `the model called an unknown tool '${name}'`));
                }
                let args;
                try {
                    args = JSON.parse(call.function.arguments || "{}");
                } catch {
                    return refuse(new ToolError(name, "args", "the model's arguments are not JSON"));
                }
                try {
                    const result = await t.call(args);
                    steps.push({ type: "tool", name, args, result });
                    return JSON.stringify(result);
                } catch (err) {
                    if (!(err instanceof ToolError)) throw err;
                    if (err.stage === "args") return refuse(err);
                    if (err.stage !== "run") throw err;
                    steps.push({ type: "tool", name, args, error: err.message });
                    return `Error: ${err.message}`;
                }
            };

            for (let calls = 0; ; calls++) {
                if (calls === maxSteps) {
                    throw new AgentError("STEP_LIMIT",
                        `the agent made ${maxSteps} model calls without a final answer`, steps);
                }
                const reply = await m.chat({
                    messages,
                    tools: definitions.length > 0 ? definitions : undefined,
                    maxTokens: options.maxTokens,
                    temperature: options.temperature,
                });
                usage.inputTokens += reply.usage.inputTokens ?? 0;
                usage.outputTokens += reply.usage.outputTokens ?? 0;
                costUsd += reply.costUsd;
                const toolCalls = reply.toolCalls ?? [];
                steps.push({ type: "model", text: reply.text, toolCalls: toolCalls.length,
                             costUsd: reply.costUsd, effect: reply.effect });
                if (toolCalls.length === 0) return { text: reply.text, steps, usage, costUsd };
                messages.push({ role: "assistant", content: reply.text === "" ? null : reply.text,
                                tool_calls: toolCalls });
                for (const call of toolCalls) {
                    messages.push({ role: "tool", tool_call_id: call.id, content: await invoke(call) });
                }
            }
        }

        return Object.freeze({ run });
    }

    const remainingOf = (left) => ({
        usd: left.nano === null ? null : left.nano / 1e9,
        tokens: left.tokens,
    });

    // Sets or lowers the run's budget and returns what remains. Limits only
    // go down; see budget::RunBudget for the rules.
    function budget(options = {}) {
        objectArg("budget", options);
        const { name, usd, tokens } = options;
        if (name !== undefined && typeof name !== "string") {
            throw new TypeError("budget: name must be a string");
        }
        if (usd !== undefined && (typeof usd !== "number" || !Number.isFinite(usd) || usd < 0)) {
            throw new TypeError("budget: usd must be a number, 0 or more");
        }
        if (tokens !== undefined && (!Number.isSafeInteger(tokens) || tokens < 0)) {
            throw new TypeError("budget: tokens must be a whole number, 0 or more");
        }
        const reply = JSON.parse(__vaern_budget_set(JSON.stringify({
            name,
            nano: usd === undefined ? null : Math.round(usd * 1e9),
            tokens: tokens ?? null,
        })));
        if (reply.error) throw new BudgetError(reply.error);
        return remainingOf(reply.remaining);
    }
    budget.remaining = () => remainingOf(JSON.parse(__vaern_budget_remaining()));
    Object.freeze(budget);

    globalThis.__vaern_ai = Object.freeze({
        model, ModelError, tool, ToolError, agent, AgentError, budget, BudgetError,
    });
})();
