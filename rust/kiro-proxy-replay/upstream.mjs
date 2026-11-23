import http from "node:http";
import { handleKiroChatMock, isKiroChatMock } from "../reference/mock-kiro-chat.mjs";

const server = http.createServer((request, response) => {
  if (isKiroChatMock(request)) return void handleKiroChatMock(request, response);
  response.writeHead(421).end();
});
server.listen(52988, "127.0.0.1", () => console.log("ready"));
