"""Fabricate a smoke account (fake ChatGPT OAuth credentials) for local relay testing.

The mock upstream accepts any bearer token; the relay's official AuthManager only needs a
parseable auth.json (id_token JWT claims + far-future access_token exp so no refresh fires).
"""
import base64
import json
import os


def b64url(obj) -> str:
    raw = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def jwt(payload: dict) -> str:
    return f"{b64url({'alg': 'none', 'typ': 'JWT'})}.{b64url(payload)}.sig"


def main() -> None:
    root = os.path.join(os.path.dirname(__file__), "accounts", "smoke1")
    os.makedirs(root, exist_ok=True)
    id_token = jwt({
        "email": "smoke@example.com",
        "exp": 2000000000,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct_smoke",
            "chatgpt_plan_type": "pro",
        },
    })
    access_token = jwt({"exp": 2000000000, "sub": "smoke"})
    auth = {
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": None,
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "rt_smoke",
            "account_id": "acct_smoke",
        },
        "last_refresh": "2026-09-18T00:00:00Z",
    }
    with open(os.path.join(root, "auth.json"), "w", encoding="utf-8") as f:
        json.dump(auth, f)
    with open(os.path.join(root, "installation_id"), "w", encoding="utf-8") as f:
        f.write("00000000-0000-4000-8000-000000000001")
    print(f"smoke account written to {root}")


if __name__ == "__main__":
    main()
