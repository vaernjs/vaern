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

    // The native side hands results over as JSON text: the reply carries the
    // provider's tool calls as nested JSON, and one parse here is simpler
    // than building nested objects through the bridge.
    globalThis.__vaern_dispatch_model = (json) => {
        const info = JSON.parse(json);
        const waiter = pending.get(info.reqId);
        if (!waiter) return;
        pending.delete(info.reqId);
        if (info.ok) waiter.resolve(info);
        else waiter.reject(new ModelError(info));
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
                reject(new ModelError(JSON.parse(refused)));
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
            const started = JSON.parse(__vaern_tool_start(reqId, name, argsJson));
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

    globalThis.__vaern_ai = Object.freeze({ model, ModelError, tool, ToolError });
})();
