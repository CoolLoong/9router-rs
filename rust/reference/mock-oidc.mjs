import http from "node:http";
import { createHash, generateKeyPairSync, sign } from "node:crypto";

const port = Number(process.argv[2]);
const issuer = `http://127.0.0.1:${port}`;
const { privateKey, publicKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const key = { ...publicKey.export({ format: "jwk" }), kid: "fixture", alg: "RS256", use: "sig" };
let challenge = null;
let nonce = null;

function send(reply, status, data) {
  reply.writeHead(status, { "Content-Type": "application/json" }).end(JSON.stringify(data));
}

http.createServer(async (req, res) => {
  const url = new URL(req.url, issuer);
  if (url.pathname === "/.well-known/openid-configuration") {
    return send(res, 200, {
      issuer,
      authorization_endpoint: `${issuer}/authorize?existing=yes`,
      token_endpoint: `${issuer}/token`,
      jwks_uri: `${issuer}/keys`,
    });
  }
  if (url.pathname === "/keys") return send(res, 200, { keys: [key] });
  if (url.pathname === "/authorize") {
    challenge = url.searchParams.get("code_challenge");
    nonce = url.searchParams.get("nonce");
    if (!challenge || !nonce || url.searchParams.get("client_id") !== "fixture-client" ||
      url.searchParams.get("code_challenge_method") !== "S256" ||
      url.searchParams.get("scope") !== "openid profile email" ||
      !url.searchParams.get("redirect_uri")?.endsWith("/api/auth/oidc/callback") ||
      url.searchParams.get("existing") !== "yes") {
      return send(res, 400, { error: "invalid fixture authorization request" });
    }
    return send(res, 200, { ok: true });
  }
  if (url.pathname !== "/token") return send(res, 404, {});
  let raw = "";
  for await (const chunk of req) raw += chunk;
  const body = new URLSearchParams(raw);
  if (body.get("client_id") !== "fixture-client" || body.get("client_secret") !== "offline-fixture-secret") {
    return send(res, 401, { error: "invalid_client", error_description: "client secret invalid" });
  }
  if (body.get("code") === "__oidc_test_invalid_code__") {
    return send(res, 400, { error: "invalid_grant", error_description: "test code invalid" });
  }
  if (body.get("grant_type") !== "authorization_code" ||
    createHash("sha256").update(body.get("code_verifier") || "").digest("base64url") !== challenge ||
    !body.get("redirect_uri")?.endsWith("/api/auth/oidc/callback")) {
    return send(res, 400, { error: "invalid_grant", error_description: "fixture code or verifier invalid" });
  }
  const now = Math.floor(Date.now() / 1000);
  const code = body.get("code");
  const claims = { iss: issuer, aud: code === "wrong-audience" ? "other-client" : "fixture-client",
    sub: "placeholder-user", nonce: code === "wrong-nonce" ? "unexpected-nonce" : nonce,
    preferred_username: "Fixture Auth", email: "fixture@example.invalid", iat: now, exp: now + 120 };
  const encode = object => Buffer.from(JSON.stringify(object)).toString("base64url");
  const signed = encode({ kid: key.kid, alg: key.alg }) + "." + encode(claims);
  const signature = sign("sha256", Buffer.from(signed), privateKey);
  if (code === "wrong-signature") signature[0] ^= 255;
  return send(res, 200, { token_type: "Bearer", id_token: signed + "." + signature.toString("base64url") });
}).listen(port, "127.0.0.1");
