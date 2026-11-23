export function assistantText(payload) {
  const choice = payload?.choices?.[0];
  const content = choice?.delta?.content || choice?.message?.content || payload?.output_text || payload?.text;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map((part) => typeof part === "string" ? part : part?.text ?? "").join("");
  }
  return "";
}

export function errorText(payload) {
  const error = payload?.error;
  if (typeof error === "string") return error;
  if (typeof error?.message === "string") return error.message;
  if (typeof payload?.message === "string") return payload.message;
  return "";
}

export async function readCompletion(response, accept) {
  if (!response.headers.get("content-type")?.includes("text/event-stream") || !response.body) {
    const payload = await response.json();
    const error = errorText(payload);
    if (error) throw new Error(error);
    accept(assistantText(payload));
    return;
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let pending = "";
  const dataLines = [];

  function dispatch() {
    const data = dataLines.join("\n");
    dataLines.length = 0;
    if (!data) return true;
    if (data.trim() === "[DONE]") return false;
    let payload;
    try {
      payload = JSON.parse(data);
    } catch {
      return true;
    }
    const error = errorText(payload);
    if (error) throw new Error(error);
    accept(assistantText(payload));
    return true;
  }

  function consumeLine(line) {
    if (!line) return dispatch();
    if (line.startsWith("data:")) dataLines.push(line.slice(5).replace(/^ /, ""));
    return true;
  }

  try {
    while (true) {
      const { done, value } = await reader.read();
      pending += done ? decoder.decode() : decoder.decode(value, { stream: true });
      let start = 0;
      for (const ending of pending.matchAll(/\r\n|[\r\n]/g)) {
        if (!done && ending[0] === "\r" && ending.index === pending.length - 1) break;
        if (!consumeLine(pending.slice(start, ending.index))) {
          await reader.cancel().catch(() => {});
          return;
        }
        start = ending.index + ending[0].length;
      }
      pending = pending.slice(start);
      if (done) {
        if (pending) consumeLine(pending);
        dispatch();
        return;
      }
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
}
