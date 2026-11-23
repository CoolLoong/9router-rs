import https from "node:https";
import http from "node:http";

const request = https.request;
https.request = function(url, ...args) {
  if (String(url) === "https://chatgpt.com/backend-api/transcribe") {
    return http.request("http://127.0.0.1:52987/codex/backend-api/transcribe", ...args);
  }
  return request.call(this, url, ...args);
};
