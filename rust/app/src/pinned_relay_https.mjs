import { resolve4, resolve6 } from "node:dns/promises";
import { Agent, request as httpsRequest } from "node:https";
import { isIP } from "node:net";
import { Readable } from "node:stream";
import { checkServerIdentity } from "node:tls";

const direct = new Agent({ keepAlive: false, maxCachedSessions: 0, proxyEnv: {} });

function publicV4(address) {
  const [a, b, c] = address.split(".").map(Number);
  return !(a === 0 || a === 10 || a === 127 || a >= 224 ||
    (a === 100 && b >= 64 && b <= 127) ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && (b === 168 || (b === 0 && (c === 0 || c === 2)) || (b === 88 && c === 99))) ||
    (a === 198 && (b === 18 || b === 19 || (b === 51 && c === 100))) ||
    (a === 203 && b === 0 && c === 113));
}

function publicV6(address) {
  if (address.includes("%")) return false;
  const [first, second, third] = address.split(":", 3).map(part => Number.parseInt(part || "0", 16));
  return (first & 0xe000) === 0x2000 &&
    !(first === 0x2001 && (second < 0x0200 || second === 0x0db8)) &&
    ![0x2002, 0x3ffe, 0x3fff].includes(first) &&
    !(first === 0x2620 && second === 0x004f && third === 0x8000);
}

function publicAddress(address) {
  if (typeof address !== "string") return false;
  const family = isIP(address);
  return family === 4 ? publicV4(address) : family === 6 && publicV6(address);
}

async function resolveAddresses(hostname) {
  const noRecords = error => {
    if (error?.code === "ENODATA") return [];
    throw error;
  };
  const [ipv4, ipv6] = await Promise.all([
    resolve4(hostname).catch(noRecords),
    resolve6(hostname).catch(noRecords),
  ]);
  return [...ipv4, ...ipv6];
}

export function pinnedHttps(resolve = resolveAddresses, send = httpsRequest) {
  return async (url, init) => {
    const addresses = await resolve(url.hostname);
    if (!addresses.length || !addresses.every(publicAddress)) {
      throw new Error("A public IP destination is required");
    }
    const headers = Object.fromEntries(init.headers);
    headers.host = url.host;
    headers["cache-control"] = "no-store";
    return await new Promise((resolveResponse, rejectResponse) => {
      const outbound = send({
        hostname: addresses[0],
        family: isIP(addresses[0]),
        port: 443,
        path: url.pathname + url.search,
        method: init.method,
        headers,
        servername: url.hostname,
        rejectUnauthorized: true,
        checkServerIdentity: (_name, certificate) => checkServerIdentity(url.hostname, certificate),
        agent: direct,
        signal: init.signal,
      }, response => {
        clearTimeout(firstReply);
        try {
          const headers = new Headers();
          for (let i = 0; i < response.rawHeaders.length; i += 2) {
            headers.append(response.rawHeaders[i], response.rawHeaders[i + 1]);
          }
          const status = response.statusCode;
          if (init.method === "HEAD" || status === 204 || status === 205 || status === 304) {
            response.resume();
            resolveResponse(new Response(null, { status, headers }));
          } else {
            resolveResponse(new Response(Readable.toWeb(response), { status, headers }));
          }
        } catch (error) {
          response.destroy();
          rejectResponse(error);
        }
      });
      const firstReply = setTimeout(() => outbound.destroy(new Error("Relay upstream timed out")), 20000);
      outbound.setTimeout(90000, () => outbound.destroy(new Error("Relay upstream stalled")));
      outbound.once("error", error => {
        clearTimeout(firstReply);
        rejectResponse(error);
      });
      outbound.once("upgrade", (_response, socket) => {
        clearTimeout(firstReply);
        socket.destroy();
        rejectResponse(new Error("Relay upstream protocol upgrade is unsupported"));
      });
      if (init.body) {
        Readable.fromWeb(init.body).once("error", error => outbound.destroy(error)).pipe(outbound);
      } else {
        outbound.end();
      }
    });
  };
}

