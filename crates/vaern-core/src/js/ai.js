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

    function start(config, options) {
        const reqId = nextReqId++;
        const call = JSON.stringify({
            messages: options.messages,
            maxTokens: options.maxTokens,
            temperature: options.temperature,
            responseFormat: options.responseFormat,
            schema: options.schema,
            schemaName: options.schemaName,
            tools: options.tools,
        });
        return new Promise((resolve, reject) => {
            pending.set(reqId, { resolve, reject });
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
                const result = {
                    text: r.text ?? "",
                    usage: { inputTokens: r.inputTokens, outputTokens: r.outputTokens },
                    costUsd: r.chargedNano / 1e9,
                    finishReason: r.finishReason ?? null,
                    effect: r.eid,
                };
                if (r.json !== null) result.json = r.json;
                return result;
            },
        });
    }

    globalThis.__vaern_ai = Object.freeze({ model, ModelError });
})();
