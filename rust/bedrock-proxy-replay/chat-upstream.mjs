import { createServer } from "node:http";
import { handleBedrockMock, isBedrockMock } from "../reference/mock-bedrock.mjs";

createServer((request, response) => {
  if (isBedrockMock(request)) {
    handleBedrockMock(request, response).catch(() => response.destroy());
  } else {
    response.writeHead(404).end();
  }
}).listen(52988, "127.0.0.1", () => console.log("ready"));
