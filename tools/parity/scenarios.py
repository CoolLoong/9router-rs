def request(path, method="GET", body=None, **options):
    return {"path":path,"method":method,"body":body,**options}


def scenarios(profile, body_for, password):
    cases = {
        "cli-show": [],
        "startup-health": [request("/api/health",key=False),request("/api/version",key=False)],
        "public-pages": [request(path,key=False) for path in ("/","/landing","/login","/dashboard")],
        "auth-session": [
            request("/dashboard",key=False),
            request("/api/auth/status",key=False),
            request("/api/settings/require-login",key=False),
            request("/api/settings",key=False),
            request("/api/auth/login","POST",{"password":"wrong-fixture-password"},key=False),
            request("/api/auth/login","POST",{"password":password},key=False),
            request("/api/auth/status",key=False),
            request("/dashboard",key=False,name="dashboard after login"),
            request("/api/auth/logout","POST",{},key=False),
            request("/api/auth/status",key=False,name="status after logout"),
        ],
        "dashboard-pages": [
            request("/dashboard/" + page,key=False)
            for page in ("endpoint","providers","providers/new","models","combos","basic-chat",
                         "cli-tools","quota","usage","profile","proxy-pools","translator","updates")
        ],
        "model-catalog": [
            request("/v1/models"),request("/api/v1/models"),request("/v1/models/cx/gpt-6.2-sol"),
            request("/api/v1/models/info"),request("/v1beta/models"),request("/api/models",key=False),
            request("/api/models/custom",key=False),request("/api/models/disabled",key=False),
        ],
        "dashboard-reads": [
            request(path,key=False) for path in (
                "/api/settings","/api/providers","/api/providers/catalog","/api/provider-nodes",
                "/api/proxy-pools","/api/combos","/api/keys","/api/usage/history","/api/usage/stats",
                "/api/usage/providers","/api/usage/request-details","/api/models/availability"
            )
        ],
        "settings-write-read": [
            request("/api/settings","PATCH",{"rtkEnabled":True},key=False),
            request("/api/settings",key=False),
            request("/api/settings","PATCH",{"rtkEnabled":False},key=False),
            request("/api/settings",key=False),
        ],
        "combo-crud": [
            request("/api/combos","POST",{"name":"parity-combo","models":["cx/gpt-6.2-sol"]},
                    key=False,capture="id"),
            request("/api/combos/{id}",key=False),
            request("/api/combos/{id}","PUT",{"name":"parity-combo","models":["dp/deepseek-v4-flash"]},key=False),
            request("/api/combos/{id}","DELETE",key=False),
            request("/api/combos",key=False),
        ],
        "gateway-key-crud": [
            request("/api/keys","POST",{"name":"parity-fixture-key"},key=False,capture="id"),
            request("/api/keys/{id}","PUT",{"name":"renamed-parity-fixture-key"},key=False),
            request("/api/keys/{id}","DELETE",key=False),
            request("/api/keys",key=False),
        ],
        "custom-model-crud": [
            request("/api/models/custom","POST",{"providerAlias":"gemini","id":"parity-custom","name":"Parity"},
                    key=False),
            request("/api/models/custom",key=False),
            request("/api/models/custom?providerAlias=gemini&id=parity-custom","DELETE",key=False),
            request("/api/models/custom",key=False),
        ],
        "model-visibility": [
            request("/api/models/disabled?providerAlias=gemini",key=False),
            request("/api/models/disabled","POST",{"providerAlias":"gemini","ids":["gemini-4-argon"]},key=False),
            request("/api/models/disabled?providerAlias=gemini",key=False),
            request("/api/models/disabled?providerAlias=gemini&id=gemini-4-argon","DELETE",key=False),
            request("/api/models/disabled?providerAlias=gemini",key=False),
        ],
        "provider-node-crud": [
            request("/api/provider-nodes","POST",{
                "name":"Parity Responses","prefix":"fixture-responses","apiType":"responses",
                "type":"openai-compatible","baseUrl":"http://127.0.0.1:52989/v1"
            },key=False,capture="id"),
            request("/api/provider-nodes/{id}","PUT",{"name":"Renamed Parity Responses"},key=False),
            request("/api/provider-nodes","GET",key=False),
            request("/api/provider-nodes/{id}","DELETE",key=False),
            request("/api/provider-nodes","GET",key=False),
        ],
        "provider-node-validate": [
            request("/api/provider-nodes/validate","POST",{
                "type":"openai-compatible","apiType":"chat","baseUrl":"http://127.0.0.1:52989/v1",
                "apiKey":"fixture-provider-key"
            },key=False)
        ],
        "provider-connection-crud": [
            request("/api/providers","POST",{"provider":profile["node"]["id"],"name":"Parity connection",
                    "apiKey":"fixture-provider-key"},key=False,capture="id"),
            request("/api/providers/{id}",key=False),
            request("/api/providers/{id}","PUT",{"name":"Renamed parity connection","isActive":False},key=False),
            request("/api/providers/{id}","DELETE",key=False),
            request("/api/providers",key=False),
        ],
        "remote-auth-and-validation": [
            request("/v1/models",key=False,headers={"X-Forwarded-For":"192.0.2.10"}),
            request("/v1/models",headers={"X-Forwarded-For":"192.0.2.10"}),
            request("/api/provider-nodes/validate","POST",{
                "type":"openai-compatible","apiType":"chat","baseUrl":"http://127.0.0.1:52989/v1",
                "apiKey":"fixture-provider-key"
            },key=False,headers={"X-Forwarded-For":"192.0.2.10"}),
        ],
        "gateway-auth-errors": [
            request("/v1/models",key=False),
            request("/v1/messages","POST",body_for("/v1/messages","cx/gpt-6.2-sol",False,"plain"),key=False),
            request("/v1/messages","POST",body_for("/v1/messages","cx/gpt-6.2-sol",False,"plain"),
                    token="wrong-fixture-key",name="incorrect gateway key"),
            request("/v1/messages","POST",{"max_tokens":32,"messages":[]},name="missing model"),
            request("/v1/messages","POST",body_for("/v1/messages","missing-provider/missing",False,"plain"),
                    name="unknown provider"),
        ],
        "claudebox-thinking": [
            request("/v1/v1/messages?beta=true","POST",body_for("/v1/v1/messages","cx/gpt-6.2-sol",True,variant),
                    name=variant)
            for variant in ("claude","adaptive","budget","disabled","tool","tool-call")
        ],
        "claude-token-count": [
            request("/v1/messages/count_tokens","POST",{
                "model":"cx/gpt-6.2-sol","messages":[{"role":"user","content":"Reply parity-pong."}]
            }),
        ],
        "native-gemini": [
            request("/v1beta/models/gemini-4-argon:" + operation,"POST",{
                "contents":[{"role":"user","parts":[{"text":"Reply parity-pong."}]}]
            })
            for operation in ("generateContent","streamGenerateContent?alt=sse")
        ],
        "upstream-errors": [
            request("/v1/chat/completions","POST",body_for("/v1/chat/completions",model,False,"reject"),
                    name=model)
            for model in ("cx/gpt-6.2-sol","dp/deepseek-v4-flash","gemini/gemini-4-argon")
        ],
        "codex-oauth-start": [
            request("/api/oauth/codex/authorize?redirectUri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
                    key=False),
        ],
        "shared-codex-protocols": [
            request(endpoint,"POST",body_for(endpoint,"cx/gpt-6.1-sol",stream,"plain"),
                    name=f"{endpoint} {'stream' if stream else 'buffered'}")
            for endpoint in profile["observedEndpoints"] for stream in (False,True)
        ],
        "native-codex-reasoning": [
            request("/v1/responses","POST",body_for("/v1/responses",model,True,"plain"),
                    name=model,headers={"User-Agent":"codex-tui/0.167.1","originator":"codex_cli_rs"})
            for model in ("cx/gpt-6.1-sol","cx/gpt-6.1-sol(xhigh)")
        ],
        "model-switch-and-usage": [
            request("/v1/v1/messages","POST",body_for("/v1/v1/messages",model,True,"plain"),name=model)
            for model in ("cx/gpt-6.2-sol","dp/deepseek-v4-flash","gemini/gemini-4-argon")
        ] + [
            request("/api/usage/history",key=False),
            request("/api/usage/stats",key=False),
            request("/api/usage/providers",key=False),
        ],
    }
    representative = ("cx/gpt-6.2-sol","dp/deepseek-v4-flash","gemini/gemini-4-argon","c-deepseek")
    for endpoint in profile["observedEndpoints"]:
        name = "transport-" + endpoint.strip("/").replace("/","-")
        cases[name] = [
            request(endpoint,"POST",body_for(endpoint,model,stream,"plain"),
                    name=f"{model} {'stream' if stream else 'buffered'}")
            for model in representative for stream in (False,True)
        ]
    for prefix in sorted({model.split("/",1)[0] for model in profile["models"]}):
        models = [model for model in profile["models"] if model.split("/",1)[0] == prefix]
        cases[f"advertised-models-{prefix}"] = [
            request("/v1/v1/messages","POST",body_for("/v1/v1/messages",model,True,"adaptive" if prefix=="cx" else "plain"),
                    name=model,timeout=24 if prefix=="oc" else 8)
            for model in models
        ]
    return cases
