import calendar
import base64
import datetime as dt
import http.server
import json
import os
import pathlib
import select
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

MODE, *ARGS = sys.argv[1:]
ROOT = pathlib.Path(__file__).resolve().parent
VERSION = json.loads((ROOT.parent.parent / "package.json").read_text())["version"]
if MODE in {"native", "strict", "full"}:
    program, CASE = ARGS
    COMMAND = [program]
else:
    CASE, = ARGS
    COMMAND = ["node", "--no-warnings", str(ROOT / "original.mjs")]
ACCESS = "fixture-provider-access"
REFRESH = "fixture-provider-refresh"
ROTATED = "fixture-provider-rotation"
NEXT = "fixture-provider-next-refresh"
PASS_KEY = "clp_fixture_quota_only"
QODER_PAT = "pt-fixture-read-only"
QODER_JOB = "jt-fixture-read-only"
KIMI_DEVICE = "0139b5c9-506c-48fb-bef1-956bfdc9880b"
CURSOR_MACHINE = "dca10f2a32414e9cb115ae2746114ebcdca10f2a32414e9cb115ae2746114ebc"
CLINE = "eyJfixture.payload.signature"
CLINE_ROTATED = "workos:eyJfixture.rotated.signature"
COPILOT = "fixture-copilot-only"
COPILOT_ROTATED = "fixture-copilot-rotation"
REQUESTS = []
COMMANDCODE_SESSIONS = set()
REDEEM_IDS = []
KIRO_PROFILE = "arn:aws:codewhisperer:us-east-1:000000000000:profile/synthetic"
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
COOKIE = None

NORMAL = {
    "rate_limit": {
        "primary_window": {"used_percent": "12.5", "reset_at": 1890000000},
        "secondary_window": {"percent_used": -2, "resetAt": "1890000000000"},
        "limit_reached": True,
    },
    "plan_type": "team",
    "review_rate_limit": {"rate_limit": {"secondary": {"used_percent": 106, "resets_at": "2027-03-04T14:03:04-05:00"}}},
    "spark_rate_limit": {"primary": {"used_percent": "8e1", "reset_at": "invalid"}, "limit_reached": True},
    "rate_limit_reset_credits": {"available_count": "4"},
}
ADDITIONAL = {
    "summary": {"plan": "pro"},
    "rate_limits": {"primary": {"percent_used": 5}},
    "additional_rate_limits": [
        {"id": "extended_review", "primary_window": {"used_percent": 20}},
        {"metered_feature": "GPT-5.3-Codex-Spark", "secondary": {"percent_used": "50"}},
    ],
}
BY_ID = {
    "rate_limits_by_limit_id": {
        "codex": {"secondary": {"percent_used": 9}},
        "code_review": {"primary_window": {"used_percent": 20}},
        "gpt-5.3-codex-spark": {"secondary": {"percent_used": 50}},
    }
}
CLAUDE = {
    "five_hour": {"utilization": 10.5, "resets_at": "2027-01-02T12:13:14+08:00"},
    "seven_day": {"utilization": 75, "resets_at": 1890000000000},
    "seven_day_sonnet": {"utilization": 30, "resets_at": "1890000000"},
    "seven_day_opus": {"utilization": "30"},
    "extra_usage": {"is_enabled": True, "used_credits": 20},
    "limits": [
        {"kind": "weekly_scoped", "percent": 132, "resets_at": "2027-01-02", "scope": {"model": {"display_name": " Fable "}}},
        {"kind": "daily_scoped", "percent": 90, "scope": {"model": {"display_name": "Skipped"}}},
    ],
    "cedar_ember": {
        "eligible": True, "next_grant_id": "paused", "cooldown_until": "2026-12-02", "weekly_resets_at": "2027-01-02",
        "grants": [
            {"id": "unused", "label": "Credit", "resets_left": "1", "resets_total": 2, "clears": ["five_hour"], "ends_at": "2027-01-02"},
            {"id": "paused", "resets_left": 2, "paused": True, "use_requires_limit": False},
            {"id": "another", "resets_left": 2, "usable_now": True, "clears": ["seven_day"]},
            {"id": "spent", "resets_left": 0},
            {"resets_left": 42},
        ],
    }
}
CREDITS = {
    "available_count": "3",
    "credits": [
        {"status": "available", "granted_at": 1890000000, "expires_at": "2027-01-02T22:00:00+02:00", "internal": "ignored"},
        {"status": "", "grantedAt": None, "expiresAt": 1890000000000},
        {"status": "used", "expires_at": "invalid"},
    ],
}
CLINE_WINDOWS = {"success": True, "data": {"limits": [
    {"type": "weekly", "percentUsed": 58},
    {"type": "monthly", "percentUsed": -30, "resetsAt": "2027-05-01"},
    {"type": "five_hour", "percentUsed": "2.5e1", "resetsAt": 1890000000},
    {"type": "unrecognized", "percentUsed": 20},
    {"type": "weekly", "percentUsed": "121.8", "resetsAt": "1890000000000"},
]}}
GITHUB_PAID = {
    "copilot_plan": "pro",
    "quota_reset_date": "2027-03-04",
    "quota_snapshots": {
        "chat": {"entitlement": 400, "remaining": 370, "unlimited": False},
        "premium_interactions": {"entitlement": 300, "remaining": None, "unlimited": False},
    },
}
GITHUB_FREE = {
    "access_type_sku": "free_limited_copilot",
    "limited_user_reset_date": 1890000000,
    "monthly_quotas": {"chat": 50, "completions": 2000},
    "limited_user_quotas": {"chat": 3},
}


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def send(self, status, value):
        body = json.dumps(value).encode()
        self.send_bytes(status, body, "application/json")

    def cursor_header(self, raw):
        def checksum(timestamp):
            last = 165
            out = bytearray()
            for index, shift in enumerate((8, 0, 24, 16, 8, 0)):
                last = ((last ^ ((timestamp >> shift) & 255)) + index) & 255
                out.append(last)
            return base64.urlsafe_b64encode(out).decode() + CURSOR_MACHINE
        now = int(time.time()) // 1000
        valid = self.headers.get("x-cursor-checksum") in {checksum(slot) for slot in range(now-1, now+2)}
        valid &= self.headers.get("Authorization") == "Bearer " + ACCESS and json.loads(raw) == {}
        valid &= self.headers.get("Accept") == self.headers.get("Content-Type") == "application/json"
        valid &= self.headers.get("x-ghost-mode") == "true" and self.headers.get("connect-protocol-version") == "1"
        if MODE != "original":
            valid &= self.headers.get("User-Agent") == "9Router"
            valid &= all(field not in self.headers for field in ["x-cursor-client-version","x-cursor-client-commit","x-cursor-client-type"])
        return valid

    def kiro_quota(self):
        uri = urllib.parse.urlsplit(self.path)
        auth = "API_KEY" if CASE == "kiro-api-key" else "EXTERNAL_IDP" if CASE == "kiro-external-q" else None
        good_auth = self.headers.get("Authorization") == "Bearer " + ACCESS and self.headers.get("TokenType") == auth
        if self.command == "GET":
            expected = {"origin":["AI_EDITOR"],"resourceType":["AGENTIC_REQUEST"]}
            if uri.path.startswith("/kiro/cw"):
                expected["isEmailRequired"] = ["true"]
            elif CASE != "kiro-api-key":
                expected["profileArn"] = [KIRO_PROFILE]
            valid = good_auth and urllib.parse.parse_qs(uri.query) == expected
        else:
            expected = {"origin":"AI_EDITOR","resourceType":"AGENTIC_REQUEST","profileArn":KIRO_PROFILE}
            actual = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            valid = good_auth and actual == expected and self.headers.get("x-amz-target") == "AmazonCodeWhispererService.GetUsageLimits"
        REQUESTS.append({"path":uri.path,"method":self.command,"valid":valid})
        if CASE == "kiro-social-denied":
            return self.send(403,{"message":"Synthetic Kiro scope denied"})
        if CASE == "kiro-idc-post" and self.command != "POST" or CASE == "kiro-external-q" and uri.path.startswith("/kiro/cw"):
            return self.send(404,{"message":"Synthetic alternate endpoint"})
        return self.send(200, {"subscriptionInfo":{"subscriptionTitle":"Kiro fixture tier"},"nextDateReset":1890000000,
            "usageBreakdownList":[{"resourceType":"AGENTIC_REQUEST","currentUsageWithPrecision":"3.50","usageLimitWithPrecision":10,
            "freeTrialInfo":{"currentUsageWithPrecision":1,"usageLimitWithPrecision":5,"freeTrialExpiry":"2027-01-01T09:00:00-05:00"}}]})

    def send_bytes(self, status, body, mime="text/plain"):
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/cursor/legacy":
            valid = self.headers.get("Authorization") == "Bearer " + ACCESS and self.headers.get("Accept") == "application/json"
            valid &= "x-cursor-checksum" not in self.headers
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE in {"cursor-http-error", "cursor-redacted"}:
                return self.send(503,{})
            if CASE == "cursor-empty":
                return self.send(200,{})
            return self.send(200,{"startOfMonth":"2027-02-03T07:00:00-08:00",
                "gpt-4":{"maxRequestUsage":"45","numRequests":13},
                "gpt-4-vision":{"maxRequestUsage":10,"numRequests":12},"ignored":{"maxRequestUsage":0}})
        if self.path == "/kimi/usages":
            valid = self.headers.get("Accept") == self.headers.get("Content-Type") == "application/json"
            if CASE.startswith("kimi-oauth-"):
                valid &= self.headers.get("x-api-key") is None and self.headers.get("Authorization") == "Bearer " + ACCESS
                valid &= self.headers.get("X-Msh-Device-Id") == KIMI_DEVICE and self.headers.get("X-Msh-Platform") == "9router"
            else:
                valid &= self.headers.get("x-api-key") == PASS_KEY and self.headers.get("Authorization") is None
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE in {"kimi-auth","kimi-oauth-denied"} or not valid:
                return self.send(401,{})
            if CASE == "kimi-permission":
                return self.send(403,{"details":[{"debug":{"reason":"REASON_FEATURE_NO_PERMISSION",
                    "localizedMessage":{"message":"Fixture account lacks a plan"}}}]})
            if CASE == "kimi-other-denial":
                return self.send_bytes(403,b"Fixture forbidden","text/plain")
            if CASE == "kimi-redacted":
                return self.send_bytes(503, ("x" * 89 + PASS_KEY).encode())
            if CASE == "kimi-invalid-json":
                return self.send_bytes(200,b"fixture not json")
            if CASE == "kimi-untracked":
                return self.send(200,{"user":{"membership":{"level":"LEVEL_SPECIAL"}}})
            return self.send(200,{"user":{"membership":{"level":"LEVEL_ADVANCED"}},
                "usage":{"Limit":"125","Used":39,"Remaining":"80.25","ResetTime":"1890000000"},
                "limits":[{"detail":{"limit":25,"remaining":"-3","resetAt":"2029-01-01T08:00:00Z"}},
                          {"detail":{"Limit":30,"Remaining":"18","reset_at":"2030-01-01T08:00:00+08:00"}}]})
        if self.path.startswith("/ollama/"):
            valid = self.headers.get("Authorization") == "Bearer " + PASS_KEY and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if not valid or CASE == "ollama-denied":
                return self.send(401, {})
            if CASE == "ollama-endpoint-error":
                return self.send(503, {})
            if CASE == "ollama-invalid-json":
                return self.send_bytes(200,b"fixture not json")
            if self.path.endswith("/usage"):
                limits = {"monthly":{"usage":"0.8755"}} if CASE == "ollama-free" else {
                    "session":{"usage":"0.193"},"weekly":{"usage":1.2},"monthly":{"usage":"invalid"},
                } if CASE == "ollama-ratios" else {}
                return self.send(200,{"limits":limits})
            return self.send(200,{} if CASE == "ollama-empty" else {"included":{
                "session":{"remaining_percent":"91.268","resets_at":"2030-12-05T09:00:00Z"},
                "weekly":{"remaining_percent":150},"allowance_usd":"100.40","balance_usd":"85.21",
                "period":{"until":"2030-12-30T09:00:00Z"}},"purchased":{"balance_usd":"12.75"}})
        if self.path.startswith("/qoder/"):
            expected = QODER_JOB if CASE.startswith(("qoder-pat", "qoder-cn-pat")) else ACCESS
            valid = self.headers.get("Authorization") == "Bearer " + expected and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if self.path.endswith("/identity"):
                expected_agent = "qodercli/1.0.0" if MODE == "original" else "9Router"
                assert self.headers.get("User-Agent") == expected_agent
                return self.send(200,{"id":"fixture-seat"})
            if CASE == "qoder-denied" or not valid:
                return self.send(403,{})
            if CASE == "qoder-invalid-json":
                return self.send_bytes(200,b"fixture not json")
            data = {"userQuota":{"total":"100","used":"22.5","remaining":"77.5"},
                    "orgResourcePackage":{"total":200,"used":25,"remaining":175,"unit":"tokens"},
                    "totalUsagePercentage":"22.50","isQuotaExceeded":False,"expiresAt":1890000000000}
            if CASE.startswith("qoder-cn"):
                data.update(expiresAt="253402214400000",isQuotaExceeded=1,
                    addOnQuota={"total":50,"used":2,"remaining":48,
                                "detailUrl":"https://qoder.com.cn/account/usage"})
            return self.send(200,data)
        if self.path in {"/opencode/go", "/opencode/zen"}:
            valid = self.headers.get("Authorization") == "Bearer " + PASS_KEY and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE.endswith("entitlement"):
                return self.send(403,{"error":{"type":"EntitlementError"}})
            if CASE.endswith("forbidden"):
                return self.send(403,{"error":{"type":"SyntheticDenied"}})
            if CASE.endswith("empty"):
                return self.send(200,{"usage":{"weekly":{"percent":"bad"}}})
            return self.send(200,{"usage":{
                "rolling":{"percent":"71.5","resetsAt":1890000000},
                "weekly":{"percent":-5,"resetsAt":"2037-06-03T09:42:51-05:00"},
                "monthly":{"percent":130,"resetsAt":"1890000000000"},
            }})
        if self.path.startswith("/commandcode/alpha/"):
            uri = urllib.parse.urlsplit(self.path)
            query = urllib.parse.parse_qs(uri.query)
            expected = {"limits":["1"]} if uri.path.endswith("/whoami") else {"orgId":["fixture-org"]}
            session = self.headers.get("x-session-id", "")
            valid = self.headers.get("Authorization") == "Bearer " + PASS_KEY and self.headers.get("Accept") == "application/json" and query == expected and self.headers.get("User-Agent") == "cli" and len(session) == 36
            COMMANDCODE_SESSIONS.add(session)
            REQUESTS.append({"path":uri.path,"method":"GET","valid":valid})
            if uri.path.endswith("/whoami"):
                if CASE == "commandcode-denied":
                    return self.send(403,{"message":"Synthetic CommandCode denial"})
                return self.send(200,{"org":{"id":"fixture-org"}})
            if uri.path.endswith("/credits"):
                return self.send(200,{"credits":{"monthlyCredits":12.2,"purchasedCredits":"7.5","freeCredits":4},
                    "windowLimits":{"fiveHour":{"used":12,"cap":20,"resetAt":"2035-01-01T08:11:12Z"},
                                    "weekly":{"used":0,"cap":60}}})
            return self.send(200,{"data":{"planId":"individual-pro-v1" if CASE == "commandcode-credits" else "custom-fixture-plan",
                "currentPeriodEnd":"1890000000"}})
        if self.path.startswith("/kiro/"):
            return self.kiro_quota()
        if self.path.startswith(("/glm/", "/minimax/")):
            acceptable_key = ACCESS if CASE == "glm-oauth" else PASS_KEY
            valid = self.headers.get("Authorization") == "Bearer " + acceptable_key and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if not valid:
                return self.send(422, {})
            if self.path.startswith("/glm/"):
                if CASE == "glm-denied":
                    return self.send(401,{})
                if CASE == "glm-cn-error":
                    return self.send(403,{})
                return self.send(200,{"data":{"level":"PRO","limits":[
                    {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":"64.5","nextResetTime":"1890000000000"},
                    {"type":"TOKENS_LIMIT","unit":6,"percentage":130},
                    {"type":"TOKENS_LIMIT","unit":1,"percentage":"invalid"},
                    {"type":"CREDIT_LIMIT","unit":1,"number":100,"percentage":-4},
                    {"type":"DISCARDED","percentage":99},
                ]}})
            assert self.headers.get("Content-Type") == "application/json"
            if CASE == "minimax-redacted":
                return self.send(200,{"base_resp":{"status_code":9123,"status_msg":"Fixture outage " + PASS_KEY}})
            if CASE == "minimax-denied":
                return self.send(200,{"baseResp":{"statusCode":"1004","statusMsg":"Invalid key"}})
            if CASE == "minimax-unavailable":
                return self.send(503,{})
            if CASE == "minimax-fallback" and "/global/" in self.path:
                return self.send(405,{})
            if CASE == "minimax-cn-empty":
                return self.send(200,{"modelRemains":[{"modelName":"unused"}]})
            return self.send(200,{"baseResp":{"statusCode":0},"modelRemains":[
                {"modelName":"general","currentIntervalRemainingPercent":81.5,"current_weekly_remaining_percent":"12.75",
                 "weeklyEndTime":"2030-02-04T15:06:00+08:00","remainsTime":"12345"},
                {"model_name":"Text-to-Speech_tts-hd","current_interval_total_count":"150","currentIntervalUsageCount":30,
                 "end_time":"1890000000","currentWeeklyTotalCount":320,"current_weekly_usage_count":99},
            ]})
        if self.path in {"/groq/models", "/vercel/credits"}:
            valid = self.headers.get("Authorization") == "Bearer " + PASS_KEY and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE.endswith("denied"):
                return self.send(403, {})
            if CASE.endswith("error"):
                return self.send_bytes(503, b"Fixture outage", "text/plain")
            if CASE.endswith("redacted"):
                return self.send_bytes(503, ("Fixture outage " + PASS_KEY).encode(), "text/plain")
            if self.path == "/vercel/credits":
                return self.send(200, {} if CASE == "vercel-empty" else {"balance":"8.25","total_used":"23.75"})
            if CASE == "groq-headers":
                self.send_response(200)
                for suffix, limit, remaining, reset in [("requests",20,17,"2m59.56s"),("tokens",300,325,"4s23ms")]:
                    self.send_header(f"x-ratelimit-limit-{suffix}",str(limit))
                    self.send_header(f"x-ratelimit-remaining-{suffix}",str(remaining))
                    self.send_header(f"x-ratelimit-reset-{suffix}",reset)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b"{}")
                return
            return self.send(200,{})
        if self.path in {"/deepseek/balance", "/synthetic/quota"}:
            valid = self.headers.get("Authorization") == "Bearer " + PASS_KEY and self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE.endswith("denied"):
                return self.send(401, {"message":"Synthetic denial fixture"})
            if self.path == "/deepseek/balance":
                if CASE == "deepseek-error":
                    return self.send_bytes(503, b"Fixture outage", "text/plain")
                return self.send(200, {"is_available":CASE != "deepseek-depleted","balance_infos":[
                    {"currency":"cny","total_balance":"143.75"},
                    {"currency":"usd","totalBalance":"-2.50"},
                    {"totalBalance":3},
                ]})
            if CASE == "synthetic-empty":
                return self.send(200, {})
            return self.send(200, {"subscription":{"limit":100,"requests":124,"renewsAt":"2027-01-03T09:01:22-05:00"}})
        if self.path == "/zed/quota":
            valid = self.headers.get("Authorization") == "42 " + ACCESS and self.headers.get("x-zed-system-id") == "fixture-zed-system"
            REQUESTS.append({"path":self.path,"method":"GET","valid":valid})
            if CASE == "zed-denied":
                return self.send(403, {"message":"Synthetic Zed authorization rejection"})
            if CASE == "zed-pro":
                return self.send(200, {"plan":{"plan_v3":"zed_pro","trial_started_at":"2027-01-01",
                    "subscription_period":{"ended_at":1890000000},"usage":{
                        "edit_predictions":{"used":"12","limit":"unlimited"},
                        "model_requests":{"used":250,"limit":{"limited":0}},
                    }}})
            if CASE == "zed-overdue":
                return self.send(200, {"plan":{"plan_v2":"SYNTHETIC_CUSTOM_PLAN","hasOverdueInvoices":True,
                    "usage":{"editPredictions":{"used":900,"limit":{"Limited":400}}}}})
            return self.send(200, {"plan":{"plan_v3":"zed_free","subscriptionPeriod":{"endedAt":"2027-03-04T14:03:04-05:00"},
                "usage":{"edit_predictions":{"used":"27.5","limit":{"limited":100}},
                    "model_requests":{"used":5,"limit":"20"}}}})
        token = self.headers.get("Authorization")
        record = {"path": self.path, "method": "GET", "valid": token in {"Bearer " + ACCESS, "Bearer " + ROTATED}, "rotated": token == "Bearer " + ROTATED}
        REQUESTS.append(record)
        if self.path == "/quota-reset/claude/profile":
            record["valid"] &= self.has_claude_headers()
            if CASE == "claude-consume-profile-denied":
                return self.send(403, {"error": "Permission denied"})
            if CASE == "claude-consume-profile-invalid":
                return self.send(200, {"organization": {"uuid": "../bad?query=yes"}})
            return self.send(200, {"organization": {"uuid": "org-fixture"}})
        if self.path == "/quota-reset/redirect-target":
            record["valid"] = False
            return self.send(500, {})
        if self.path == "/clinepass/quota":
            self.clinepass_quota(token, record)
            return
        if self.path in {"/github/quota", "/github/copilot"}:
            self.github_quota(token, record)
            return
        if self.path == "/codex/quota":
            record["valid"] &= self.headers.get("Accept") == "application/json" and not self.headers.get("ChatGPT-Account-ID")
            if CASE in {"codex-rejected", "codex-no-refresh"} and not record["rotated"]:
                return self.send(401, {"message": "Unauthorized"})
            data = {"codex-empty": {}, "codex-additional": ADDITIONAL, "codex-by-id": BY_ID}.get(CASE, NORMAL)
            return self.send(200, data)
        if self.path == "/codex/credits":
            record["valid"] &= self.headers.get("OpenAI-Beta") == "codex-1" and self.headers.get("originator") == "codex_cli_rs" and self.headers.get("Accept") == "application/json"
            record["valid"] &= self.headers.get("ChatGPT-Account-ID") == {"codex-reset-access": "account-id", "codex-reset-rejected":"fallback-workspace"}.get(CASE, "workspace-preferred")
            if CASE == "codex-reset-rejected" and not record["rotated"]:
                return self.send(401, {"error": {"message": "Refresh required: Unauthorized."}})
            if CASE == "codex-reset-failed":
                return self.send(503, {"detail": "Provider quota status unavailable"})
            if CASE == "codex-reset-redacted":
                return self.send(503, {"detail": "request included " + ACCESS})
            return self.send(200, CREDITS)
        if self.path == "/claude/quota?cedar_ember=1":
            record["valid"] &= self.headers.get("Anthropic-Beta") == "oauth-2025-04-20" and self.headers.get("Anthropic-Version") == "2023-06-01" and self.headers.get("User-Agent") == "claude-cli/2.1.322 (external, cli)"
            if CASE in {"claude-legacy", "claude-admin", "claude-cooldown"}:
                return self.send({"claude-legacy":403,"claude-admin":401,"claude-cooldown":429}[CASE], {})
            if CASE == "claude-ineligible":
                return self.send(200, {"cedar_ember": {"eligible":False,"grants":[]}})
            return self.send(200, CLAUDE)
        if self.path == "/claude/settings":
            record["valid"] &= self.headers.get("Anthropic-Version") == "2023-06-01" and self.headers.get("Anthropic-Beta") is None
            if CASE == "claude-admin":
                return self.send(401, {"error": "Denied"})
            return self.send(200, {"plan":"Team","organization_id":"org-fixture","organization_name":"Synthetic Account"})
        if self.path == "/claude/organizations/org-fixture/usage":
            record["valid"] &= self.headers.get("Anthropic-Version") == "2023-06-01" and self.headers.get("Anthropic-Beta") is None
            return self.send(200, {"legacy_window":{"used":2, "total":11}})
        record["valid"] = False
        self.send(404, {})

    def do_POST(self):
        if self.path.startswith("/kiro/"):
            return self.kiro_quota()
        if self.path in {"/gemini/quota", "/gemini/assist"}:
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            expected = {"metadata":{"ideType":9,"platform":3,"pluginType":2}} if self.path.endswith("assist") else {
                "project":"projects/fixture-stored" if CASE in {"gemini-stored","gemini-denied"} else "fixture-discovered",
            }
            valid = self.headers.get("Authorization") == "Bearer " + ACCESS and body == expected
            REQUESTS.append({"path":self.path,"method":"POST","valid":valid})
            if self.path.endswith("assist"):
                info = {"currentTier":{"name":"Gemini fixture tier"}}
                if CASE != "gemini-missing-project":
                    info["cloudaicompanionProject"] = {"id":" fixture-discovered "}
                return self.send(200, info)
            if CASE == "gemini-denied":
                return self.send(403, {"message":"Synthetic permission denied"})
            return self.send(200, {"buckets":[
                {"modelId":"fixture-model-a","remainingFraction":"0.8755","resetTime":1890000000},
                {"modelId":"fixture-model-b","remainingFraction":0.16,"resetTime":"1890000000000"},
                {"modelId":"fixture-model-a","remainingFraction":0.5,"resetTime":"2027-03-04T14:03:04-05:00"},
                {"modelId":"no-limits","remainingFraction":None},
                {"remainingFraction":0.95},
            ]})
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path in {"/cursor/period", "/cursor/plan"}:
            valid = self.cursor_header(raw)
            REQUESTS.append({"path":self.path,"method":"POST","valid":valid})
            if not valid:
                return self.send(422,{})
            if self.path == "/cursor/plan":
                return self.send(503,{}) if CASE == "cursor-percent" else self.send(200,{"planInfo":{"planName":" Pro + "}})
            if CASE == "cursor-denied":
                return self.send(401,{})
            if CASE in {"cursor-http-error", "cursor-redacted"}:
                error = "Fixture error" if CASE == "cursor-http-error" else "Fixture error: " + ACCESS
                return self.send_bytes(502,error.encode(),"text/plain")
            if CASE == "cursor-empty":
                return self.send(200,{"displayMessage":"Fixture plan not tracked"})
            if CASE == "cursor-legacy":
                return self.send(503,{})
            reset = "1890000000000"
            if CASE == "cursor-percent":
                return self.send(200,{"billingCycleEnd":reset,"planUsage":{"totalPercentUsed":"122.50","autoPercentUsed":-5},
                    "spendLimitUsage":{"pooledLimit":2000,"pooledUsed":888,"pooledRemaining":1112}})
            return self.send(200,{"billingCycleEnd":reset,"planUsage":{"limit":"2000","includedSpend":"449","remaining":1551,
                "autoPercentUsed":27.5,"apiPercentUsed":"5.5"},
                "spendLimitUsage":{"individualLimit":300,"individualUsed":301,"pooledLimit":1000,"pooledUsed":999}})
        if self.path == "/ollama/me":
            valid = raw == b"" and self.headers.get("Content-Length") == "0" and self.headers.get("Authorization") == "Bearer " + PASS_KEY
            valid &= self.headers.get("Accept") == "application/json"
            REQUESTS.append({"path":self.path,"method":"POST","valid":valid})
            if CASE == "ollama-balance" or not valid:
                return self.send(503,{})
            return self.send(200,{"Plan":"FrEe" if CASE == "ollama-free" else "TEAM", "CreatedAt":"2023-01-31T10:20:30.999Z"})
        if self.path == "/github/token":
            valid = self.headers.get("Content-Type") == "application/x-www-form-urlencoded" and (
                dict(urllib.parse.parse_qsl(raw.decode())) == {
                    "grant_type": "refresh_token", "client_id": "Iv1.b507a08c87ecfe98", "refresh_token": REFRESH,
                })
            REQUESTS.append({"path": self.path, "method": "POST", "valid": valid})
            return self.send(200, {"access_token": ROTATED, "refresh_token": NEXT, "expires_in": 28800})
        body = json.loads(raw)
        if self.path.startswith("/qoder/") and self.path.endswith("/exchange"):
            expected_agent = "qodercli/1.0.0" if MODE == "original" else "9Router"
            valid = body == {"personal_token":QODER_PAT} and self.headers.get("Content-Type") == "application/json"
            valid &= self.headers.get("Accept") == "application/json" and self.headers.get("User-Agent") == expected_agent
            REQUESTS.append({"path":self.path,"method":"POST","valid":valid})
            if CASE == "qoder-pat-denied" or not valid:
                return self.send(403,{"error":"Fixture exchange denied"})
            return self.send(200,{"token":QODER_JOB,"expires_in":86400000})
        if self.path == "/quota-reset/codex/consume":
            return self.codex_consume(body)
        if self.path == "/quota-reset/claude/organizations/org-fixture/reset_rate_limits":
            return self.claude_consume(body)
        if self.path == "/cline/api/v1/auth/refresh":
            valid = self.headers.get("Accept") == "application/json" and body == {
                "refreshToken": REFRESH, "grantType": "refresh_token", "clientType": "extension",
            }
            REQUESTS.append({"path": self.path, "method": "POST", "valid": valid})
            return self.send(200, {"success": True, "data": {
                "accessToken": CLINE_ROTATED, "refreshToken": NEXT, "expiresAt": "2099-03-04T00:00:00Z",
            }})
        provider = self.path.rsplit("/", 1)[0].strip("/")
        valid = self.path in {"/codex/token", "/claude/token"} and body == {
            "grant_type": "refresh_token", "client_id": f"fixture-{provider}-client", "refresh_token": REFRESH
        }
        REQUESTS.append({"path": self.path, "method": "POST", "valid": valid})
        if CASE == "proxy-added-on-refresh":
            with sqlite3.connect(PROFILE / "db/data.sqlite") as conn:
                conn.execute("UPDATE providerConnections SET data=json_set(data,'$.providerSpecificData.proxyPoolId','fixture-pool') WHERE id='fixture'")
        self.send(200, {"access_token": ROTATED, "refresh_token": NEXT, "expires_in": 28800})

    def has_claude_headers(self):
        return (self.headers.get("Anthropic-Beta") == "oauth-2025-04-20"
                and self.headers.get("Anthropic-Version") == "2023-06-01"
                and self.headers.get("User-Agent") == "claude-cli/2.1.322 (external, cli)"
                and self.headers.get("Content-Type") == "application/json")

    def save_redeem(self, candidate):
        try:
            parsed = uuid.UUID(candidate)
            valid = parsed.version == 4 and str(parsed) == candidate and candidate != "caller-redemption"
        except (ValueError, TypeError, AttributeError):
            valid = False
        if REDEEM_IDS:
            valid &= REDEEM_IDS[0] == candidate
        REDEEM_IDS.append(candidate)
        return valid

    def codex_consume(self, body):
        token = self.headers.get("Authorization")
        rotated = token == "Bearer " + ROTATED
        valid = (token in {"Bearer " + ACCESS, "Bearer " + ROTATED}
                 and self.headers.get("Accept") == "application/json"
                 and self.headers.get("Content-Type") == "application/json"
                 and self.headers.get("ChatGPT-Account-ID") is None
                 and set(body) == {"redeem_request_id"} and self.save_redeem(body["redeem_request_id"]))
        REQUESTS.append({"path":self.path, "method":"POST", "valid":valid, "rotated":rotated})
        if CASE in {"codex-consume-rejected", "codex-consume-access-denied"} and not rotated:
            return self.send(401, {"code":"unauthorized", "message":"Authentication expired"})
        if CASE == "codex-consume-no-credit":
            return self.send(200, {"code":"no_credit", "windows_reset":0})
        if CASE == "codex-consume-policy":
            return self.send(403, {"code":"denied", "message":"Permission denied"})
        if CASE == "codex-consume-unavailable":
            return self.send(503, {"code":"temporarily_unavailable", "message":"Not available"})
        if CASE == "codex-consume-ambiguous":
            return self.send(500, {"code":"authentication", "message":"Authentication service unavailable"})
        if CASE == "codex-consume-malformed":
            return self.send_bytes(200, b"invalid-json")
        if CASE == "codex-consume-redirect":
            self.send_response(307)
            self.send_header("Location", "/quota-reset/redirect-target")
            self.end_headers()
            return
        if CASE == "codex-consume-disconnected":
            self.close_connection = True
            return
        if CASE == "codex-consume-redacted":
            return self.send(200, {"code":"reset", "credit":{"label":"reflected " + ACCESS}})
        return self.send(200, {"code":"reset", "windows_reset":"2", "credit":{"fixture":True}})

    def claude_consume(self, body):
        token = self.headers.get("Authorization")
        valid = (token == "Bearer " + ACCESS and self.has_claude_headers()
                 and set(body) == {"program", "grant_id", "request_id"}
                 and body["program"] == "cedar_ember" and body["grant_id"] == "fixture-grant_123"
                 and self.save_redeem(body["request_id"]))
        REQUESTS.append({"path":self.path, "method":"POST", "valid":valid})
        if CASE == "claude-consume-spent":
            return self.send(409, {"result":"unavailable","reason":"already_spent","resets_left":0})
        if CASE == "claude-consume-throttled":
            return self.send(429, {"error":{"message":"Try again later"}})
        if CASE == "claude-consume-unavailable":
            return self.send(503, {"reason":"temporarily_unavailable"})
        if CASE == "claude-consume-redacted":
            return self.send(409, {"error":{"message":"reflected " + ACCESS}})
        return self.send(200, {"result":"reset", "resets_left":1})

    def clinepass_quota(self, token, record):
        oauth = CASE in {"clinepass-oauth", "clinepass-rejected", "clinepass-proactive"}
        record["rotated"] = token == "Bearer " + CLINE_ROTATED
        record["valid"] = self.headers.get("Accept") == "application/json"
        if oauth:
            record["valid"] &= token in {"Bearer workos:" + CLINE, "Bearer " + CLINE_ROTATED}
            record["valid"] &= self.headers.get("HTTP-Referer") == "https://cline.bot"
            record["valid"] &= self.headers.get("X-Title") == "Cline"
            record["valid"] &= self.headers.get("X-Client-Type") == "9router"
            record["valid"] &= self.headers.get("X-Client-Version") == self.headers.get("X-Core-Version") == VERSION
            record["valid"] &= self.headers.get("User-Agent") == f"9Router/{VERSION}"
            record["valid"] &= self.headers.get("X-Platform") == sys.platform
            record["valid"] &= self.headers.get("X-Is-MultiRoot") == "false"
            version = self.headers.get("X-Platform-Version", "")
            record["valid"] &= version == "unknown" if MODE in {"native", "strict"} else version.startswith("v")
        else:
            record["valid"] &= token == "Bearer " + PASS_KEY and self.headers.get("X-Client-Type") is None
        if CASE == "clinepass-expired" or CASE == "clinepass-rejected" and not record["rotated"]:
            return self.send(403, {})
        if CASE == "clinepass-limited":
            return self.send_bytes(429, ("Rate limit " + "x" * 150).encode())
        if CASE == "clinepass-nonjson":
            return self.send_bytes(200, b"not-json")
        if CASE == "clinepass-empty":
            return self.send(200, {"data": {"limits": []}})
        if CASE == "clinepass-redacted":
            return self.send_bytes(502, ("Provider sent " + PASS_KEY).encode())
        return self.send(200, CLINE_WINDOWS)

    def github_quota(self, token, record):
        record["valid"] = token in {"token " + ACCESS, "token " + ROTATED}
        record["rotated"] = token == "token " + ROTATED
        record["valid"] &= self.headers.get("Accept") == "application/json"
        if self.path == "/github/copilot":
            record["valid"] &= self.headers.get("X-GitHub-Api-Version") == "2025-04-01"
            record["valid"] &= self.headers.get("Editor-Version") == "vscode/1.110.0"
            if CASE == "github-proactive-rotates" and not record["rotated"]:
                return self.send(401, {"error": "expired"})
            return self.send(200, {"token": COPILOT_ROTATED, "expires_at": 4102444800})
        record["valid"] &= self.headers.get("X-GitHub-Api-Version") == "2022-11-28"
        record["valid"] &= self.headers.get("User-Agent") == "GitHubCopilotChat/0.26.7"
        record["valid"] &= self.headers.get("Editor-Version") == "vscode/1.100.0"
        record["valid"] &= self.headers.get("Editor-Plugin-Version") == "copilot-chat/0.26.7"
        if CASE == "github-error":
            return self.send_bytes(401, b"Request rejected")
        if CASE == "github-redacted":
            return self.send_bytes(503, ("Provider sent " + ACCESS).encode())
        if CASE == "github-empty":
            return self.send(200, {})
        if CASE == "github-free":
            return self.send(200, GITHUB_FREE)
        return self.send(200, GITHUB_PAID)


def setup(root):
    if CASE in {"missing-account", "codex-consume-missing-account", "claude-consume-missing-account"}:
        return
    provider = "claude" if CASE.startswith("claude") or CASE == "reset-wrong-provider" else "codex"
    auth = "access_token" if CASE in {"codex-reset-access","codex-reset-missing-token"} else ("apikey" if CASE in {"reset-ineligible", "quota-ineligible"} else "oauth")
    if CASE in {"codex-consume-access", "codex-consume-access-denied", "codex-consume-missing-token"}:
        auth = "access_token"
    if CASE in {"codex-consume-ineligible", "claude-consume-ineligible"}:
        auth = "apikey"
    if CASE == "codex-consume-wrong-provider":
        provider = "claude"
    if CASE.startswith("github"):
        provider = "github"
        auth = "apikey" if CASE == "github-ineligible" else "oauth"
    if CASE.startswith("gemini-"):
        provider = "gemini-cli"
    if CASE.startswith("zed-"):
        provider = "zed"
    if CASE.startswith("kiro-"):
        provider = "kiro"
        auth = "api_key" if CASE == "kiro-api-key" else "oauth"
    if CASE.startswith(("deepseek-", "groq-", "kimi-", "ollama-", "synthetic-")):
        provider = CASE.split("-",1)[0]
        auth = "oauth" if CASE.startswith("kimi-oauth-") else "apikey"
    if CASE.startswith("vercel-"):
        provider, auth = "vercel-ai-gateway", "apikey"
    if CASE.startswith(("glm-", "minimax-")):
        provider = CASE.split("-",1)[0] + ("-cn" if "-cn-" in CASE else "")
        auth = "oauth" if CASE == "glm-oauth" else "apikey"
    if CASE == "iflow-tracked":
        provider, auth = "iflow", "oauth"
    if CASE.startswith("qoder-"):
        provider = "qoder-cn" if CASE.startswith("qoder-cn-") else "qoder"
        auth = "apikey" if "-pat-" in CASE or CASE == "qoder-no-access" else "oauth"
    if CASE.startswith("cursor-"):
        provider, auth = "cursor", "oauth"
    if CASE.startswith("commandcode-"):
        provider = "commandcode"
        auth = "apikey"
    if CASE.startswith(("opencode-go-", "opencode-zen-")):
        provider = CASE.rsplit("-",1)[0]
        auth = "apikey"
    if CASE.startswith("clinepass"):
        provider = "clinepass"
        auth = {
            "clinepass-underscore": "api_key",
            "clinepass-oauth": "oauth",
            "clinepass-rejected": "oauth",
            "clinepass-proactive": "oauth",
        }.get(CASE, "apikey")
    now = dt.datetime.now(dt.timezone.utc)
    def iso(value):
        return value.isoformat(timespec="milliseconds").replace("+00:00","Z")
    ttl = 30 if CASE.endswith("-proactive") or CASE == "proxy-added-on-refresh" else 28800
    metadata = {"workspaceId":"workspace-preferred","accountId":"account-id","chatgptAccountId":"fallback-workspace"}
    if CASE == "codex-reset-access":
        del metadata["workspaceId"]
    if CASE == "codex-reset-rejected":
        del metadata["workspaceId"]
        del metadata["accountId"]
    if CASE.endswith("proxied"):
        metadata["proxyPoolId"] = "fixture-pool"
    fields = {"accessToken":ACCESS, "refreshToken":REFRESH,"expiresAt":iso(now+dt.timedelta(seconds=ttl)),
              "lastRefreshAt":iso(now),"providerSpecificData":metadata}
    if auth != "oauth" or CASE == "codex-no-refresh":
        del fields["refreshToken"]
    if CASE in {"codex-reset-missing-token", "codex-consume-missing-token"}:
        del fields["accessToken"]
    if provider == "clinepass":
        fields["accessToken"] = CLINE
        if auth != "oauth":
            fields["apiKey"] = " " + PASS_KEY + " "
        if CASE == "clinepass-missing":
            del fields["accessToken"]
            del fields["apiKey"]
        if CASE == "clinepass-malformed-key":
            fields["apiKey"] = {"bad": True}
    if provider == "github":
        if not CASE.startswith("github-proactive"):
            metadata.update(copilotToken=COPILOT, copilotTokenExpiresAt=4102444800)
        if CASE == "github-missing":
            del fields["accessToken"]
    if provider == "gemini-cli":
        if CASE in {"gemini-stored", "gemini-denied"}:
            metadata["projectId"] = "discarded-fixture-project"
            fields["projectId"] = {"id":" projects/fixture-stored "}
        if CASE == "gemini-missing-token":
            del fields["accessToken"]
    if provider == "zed":
        fields.pop("refreshToken", None)
        metadata.update(userId="42",systemId="fixture-zed-system")
        if CASE == "zed-missing-user":
            del metadata["userId"]
        if CASE == "zed-missing-token":
            del fields["accessToken"]
    if provider == "kiro":
        fields.pop("refreshToken",None)
        metadata["authMethod"] = {"kiro-api-key":"api_key","kiro-idc-post":"idc",
            "kiro-external-q":"external_idp","kiro-social-denied":"google"}[CASE]
        if auth != "api_key":
            metadata["profileArn"] = KIRO_PROFILE
    if provider in {"deepseek", "synthetic", "commandcode", "opencode-go", "opencode-zen"} and not CASE.endswith("missing"):
        fields["apiKey"] = " " + PASS_KEY + " "
    if provider in {"groq", "kimi", "ollama", "vercel-ai-gateway"} and auth == "apikey":
        fields["apiKey"] = " " + PASS_KEY + " " if provider == "groq" else PASS_KEY
    if provider in {"glm","glm-cn","minimax","minimax-cn"} and CASE != "glm-oauth":
        fields["apiKey"] = PASS_KEY
        fields.pop("accessToken", None)
    if provider == "iflow":
        fields["expiresAt"] = iso(now + dt.timedelta(days=15))
        fields.pop("refreshToken", None)
    if provider in {"qoder","qoder-cn"}:
        fields.pop("refreshToken", None)
        if auth == "apikey":
            fields.pop("accessToken", None)
            if "-pat-" in CASE:
                fields["apiKey"] = QODER_PAT
    if provider == "kimi" and auth == "apikey":
        fields.pop("accessToken", None)
    if provider == "kimi" and auth == "oauth":
        fields.pop("refreshToken", None)
        metadata.update(authMethod="device_code",deviceId=KIMI_DEVICE)
    if provider == "cursor":
        fields.pop("refreshToken",None)
        fields["accessToken"] = "fixture-user::" + ACCESS
        metadata.update(machineId=CURSOR_MACHINE,ghostMode=False)
        if CASE == "cursor-missing-token":
            fields.pop("accessToken",None)
    if CASE.endswith("missing-key"):
        fields.pop("apiKey",None)
    with sqlite3.connect(root/"db/data.sqlite") as conn:
        conn.execute("""INSERT INTO providerConnections(id,provider,authType,priority,isActive,data,createdAt,updatedAt)
                        VALUES ('fixture',?,?,1,1,?,?,?)""", (provider,auth,json.dumps(fields),iso(now),iso(now)))


def response(port, path, authorized=True, method="GET", data=None):
    url = f"http://127.0.0.1:{port}/api/usage/fixture{path}"
    headers = {}
    if authorized:
        headers = {"Cookie": COOKIE} if MODE == "full" else {"x-replay-admin": "fixture"}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data).encode()
    req = urllib.request.Request(url, headers=headers, method=method, data=data)
    try:
        reply = OPENER.open(req, timeout=7)
    except urllib.error.HTTPError as error:
        reply = error
    with reply:
        raw = reply.read()
        try:
            data = json.loads(raw)
        except json.JSONDecodeError:
            data = raw.decode()
        return {"status":reply.status, "body":data}


def run(port):
    if CASE.startswith(("codex-consume", "claude-consume")):
        return run_consume(port)
    path = "/codex-reset-credits" if CASE.startswith("codex-reset") or CASE.startswith("reset-") else ""
    started = dt.datetime.now(dt.timezone.utc)
    replies = [response(port, path, CASE != "unauthorized")]
    ended = dt.datetime.now(dt.timezone.utc)
    if CASE == "groq-headers":
        quotas = replies[0]["body"]["quotas"]
        for label, delay in [("Requests",179560),("Tokens",4023)]:
            at = dt.datetime.fromisoformat(quotas[label]["resetAt"].replace("Z","+00:00"))
            assert started + dt.timedelta(milliseconds=delay-5) <= at <= ended + dt.timedelta(milliseconds=delay+5)
            quotas[label]["resetAt"] = f"now+{delay}ms"
    if CASE in {"minimax-counts", "minimax-cn-counts", "minimax-fallback"}:
        quota = replies[0]["body"]["quotas"]["M-series (5h)"]
        at = dt.datetime.fromisoformat(quota["resetAt"].replace("Z","+00:00"))
        assert started + dt.timedelta(milliseconds=12340) <= at <= ended + dt.timedelta(milliseconds=12350)
        quota["resetAt"] = "now+12345ms"
    if CASE in {"claude-cache", "claude-cooldown"}:
        replies.append(response(port, path))
        replies.append(response(port, "?force=1"))
    if CASE == "qoder-pat-cache":
        replies.append(response(port, path))
        assert [r["method"] for r in REQUESTS] == ["POST","GET","GET","GET"], REQUESTS
    if CASE.startswith("qoder-"):
        assert all(r["valid"] for r in REQUESTS), REQUESTS
        assert all(secret not in json.dumps(replies) for secret in [ACCESS,QODER_PAT,QODER_JOB])
    if CASE == "ollama-free":
        quota = replies[0]["body"]["quotas"]["Monthly"]
        year, month = started.year, started.month
        expected = dt.datetime(year, month, calendar.monthrange(year, month)[1], 10, 20, 30, tzinfo=dt.timezone.utc)
        if expected <= started:
            year, month = (year + 1, 1) if month == 12 else (year, month + 1)
            expected = dt.datetime(year, month, calendar.monthrange(year, month)[1], 10, 20, 30, tzinfo=dt.timezone.utc)
        assert quota["resetAt"] == expected.isoformat(timespec="milliseconds").replace("+00:00", "Z")
        quota["resetAt"] = "next UTC signup anniversary"
    if CASE.startswith("ollama-"):
        assert all(r["valid"] for r in REQUESTS), REQUESTS
    if CASE.endswith("redacted"):
        assert all(key not in json.dumps(replies) for key in [ACCESS, PASS_KEY])
        assert "[redacted]" in json.dumps(replies)
    if CASE.startswith("kimi-"):
        assert all(r["valid"] for r in REQUESTS), REQUESTS
    if CASE.startswith("cursor-"):
        assert all(r["valid"] for r in REQUESTS), REQUESTS
    if CASE == "proxied":
        assert replies[0] == {"status": 503, "body": {"error": "Account quota proxy is unavailable"}} and not REQUESTS
    elif CASE.endswith("proxied") or CASE == "clinepass-malformed-key":
        assert replies[0]["status"] == 501 and not REQUESTS
    if CASE == "proxy-added-on-refresh":
        assert replies[0] == {"status": 503, "body": {"error": "Account quota proxy is unavailable"}}
        assert [request["method"] for request in REQUESTS] == ["POST"]
    return replies


def run_consume(port):
    codex = CASE.startswith("codex-")
    path = "/codex-reset-credits" if codex else "/claude-reset"
    data = ({"redeem_request_id":"caller-redemption"} if codex else
            {"grantId":"../not-a-grant" if CASE == "claude-consume-invalid" else "fixture-grant_123"})
    replies = []
    if CASE == "claude-consume-cache":
        replies.append(response(port, ""))
    reply = response(port, path, CASE != "codex-consume-unauthorized", "POST", data)
    if "redeemRequestId" in reply["body"]:
        assert len(REDEEM_IDS) >= 1 and reply["body"]["redeemRequestId"] == REDEEM_IDS[0]
        reply["body"]["redeemRequestId"] = "server-generated-uuid-v4"
    replies.append(reply)
    if CASE == "claude-consume-cache":
        replies.append(response(port, ""))
        assert [request["path"] for request in REQUESTS].count("/claude/quota?cedar_ember=1") == 2
    assert all(request["valid"] for request in REQUESTS)
    if CASE in {"codex-consume-ambiguous", "codex-consume-malformed", "codex-consume-redirect", "codex-consume-disconnected"}:
        assert len(REDEEM_IDS) == 1 and len(REQUESTS) == 1
        assert reply["status"] == (500 if CASE.endswith(("malformed","disconnected")) else 502)
    if CASE == "claude-consume-profile-invalid":
        assert reply["status"] == 500 and len(REQUESTS) == 1
    if CASE.endswith("redacted"):
        assert ACCESS not in json.dumps(replies) and "[redacted]" in json.dumps(replies)
    if MODE == "full":
        expected_status, request_count = {
            "codex-consume-access": (200, 1),
            "codex-consume-unauthorized": (401, 0),
            "claude-consume": (200, 2),
        }[CASE]
        assert reply["status"] == expected_status and len(REQUESTS) == request_count
    return replies


def wait_port(server):
    limit = time.monotonic()+7
    marker = None
    output = bytearray()
    while not marker and time.monotonic() < limit:
        if not select.select([server.stdout],[],[],1)[0]:
            continue
        chunk = os.read(server.stdout.fileno(),4096)
        if not chunk:
            break
        output.extend(chunk)
        marker = next((line for line in output.split(b"\n")[:-1] if line.startswith(b"ready ")),None)
    assert marker, "reference failed to become ready"
    return int(marker.split()[1])


def login(server, port):
    base = f"http://127.0.0.1:{port}"
    limit = time.monotonic() + 7
    while time.monotonic() < limit and server.poll() is None:
        try:
            with OPENER.open(base + "/api/health", timeout=.4) as reply:
                if reply.status == 200:
                    break
        except OSError:
            time.sleep(.02)
    else:
        raise AssertionError("native server failed to become ready")
    request = urllib.request.Request(base + "/api/auth/login", b'{"password":"123456"}',
                                    method="POST", headers={"Content-Type":"application/json"})
    with OPENER.open(request, timeout=6) as reply:
        assert reply.status == 200
        return reply.headers["Set-Cookie"].split(";", 1)[0]


with tempfile.TemporaryDirectory(prefix="9router-account-quota-fixture-") as directory:
    root = PROFILE = pathlib.Path(directory)
    fixed_port = 52989 if CASE in {"clinepass-rejected", "clinepass-proactive"} or CASE.startswith(("codex-consume", "claude-consume")) else 0
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", fixed_port), Mock)
    threading.Thread(target=lambda:upstream.serve_forever(poll_interval=.02),daemon=True).start()
    base = f"http://127.0.0.1:{upstream.server_port}"
    environment = dict(os.environ, HOME=directory, DATA_DIR=directory, NO_PROXY="localhost,127.0.0.1",
                       NINEROUTER_MIGRATION_FIXTURE="1")
    arguments = [directory, base]
    if MODE == "full":
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        path = root / "empty-path"
        path.mkdir()
        environment = {"PATH":str(path), "HOME":directory, "DATA_DIR":directory,
                       "PORT":str(port), "NINEROUTER_MIGRATION_FIXTURE":"1"}
        arguments = ["--no-browser"]
    try:
        with subprocess.Popen(COMMAND+arguments,cwd=root,env=environment,stdout=subprocess.PIPE,stderr=subprocess.PIPE) as server:
            try:
                if MODE == "full":
                    COOKIE = login(server, port)
                else:
                    port = wait_port(server)
                setup(root)
                replies = run(port)
                with sqlite3.connect(root/"db/data.sqlite") as conn:
                    row = conn.execute("SELECT data FROM providerConnections WHERE id='fixture'").fetchone()
                    storage = json.loads(row[0]) if row else {}
            finally:
                if server.poll() is None:
                    server.terminate()
                try:
                    server.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.communicate(timeout=3)
    finally:
        upstream.shutdown()
        upstream.server_close()
    assert all(req["valid"] for req in REQUESTS)
    if CASE.startswith("commandcode-"):
        assert len(COMMANDCODE_SESSIONS) <= 1
        REQUESTS.sort(key=lambda req: req["path"])
    snapshot = {
        "replies": replies, "outbound": REQUESTS,
        "accessRotated": storage.get("accessToken") in {ROTATED, CLINE_ROTATED},
        "refreshRotated": storage.get("refreshToken") == NEXT,
    }
    if CASE.startswith("github"):
        snapshot["copilotRotated"] = storage.get("providerSpecificData", {}).get("copilotToken") == COPILOT_ROTATED
    print(json.dumps(snapshot, sort_keys=True))
