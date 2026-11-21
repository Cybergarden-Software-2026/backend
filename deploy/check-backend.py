import json
import subprocess
import sys
from http.cookies import SimpleCookie
from urllib.error import HTTPError
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


BASE = "http://127.0.0.1:8001"
SESSION = "/api/v1/auth/coordinator/session"
SNAPSHOT = "/api/v1/snapshot"
COOKIE = "__Host-underctrl-coordinator"


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


opener = build_opener(ProxyHandler({}), NoRedirect())


def request(path, *, method="GET", headers=None, data=None):
    return opener.open(Request(BASE + path, data, headers or {}, method=method), timeout=5)


def session_status():
    try:
        with request(SESSION) as response:
            status = json.load(response)
            required = status["required"]
            if not isinstance(required, bool):
                raise ValueError("Invalid session status")
            return required, status.get("mode")
    except HTTPError as error:
        if error.code != 404:
            raise
        error.close()
        return False, None  # The previous image may predate coordinator authentication.


def coordinator_config(container):
    config = subprocess.check_output(
        ["docker", "container", "inspect", "--format", "{{json .Config.Env}}", container],
        stderr=subprocess.DEVNULL,
        timeout=10,
    )
    env = dict(entry.split("=", 1) for entry in json.loads(config))
    accounts = env.get("COORDINATOR_ACCOUNTS_ENABLED", "").strip().lower()
    return accounts in ("1", "true", "t", "yes", "y", "on"), env.get("COORDINATOR_ACCESS_KEY")


def anonymous_access_denied():
    try:
        with request(SNAPSHOT):
            return False
    except HTTPError as error:
        error.close()
        return error.code == 401


def session_cookie(key):
    payload = json.dumps({"key": key, "remember": False}).encode()
    with request(SESSION, method="POST", headers={"Content-Type": "application/json"}, data=payload) as response:
        if json.load(response)["authenticated"] is not True:
            raise ValueError("Authentication failed")
        cookie = SimpleCookie(response.headers["Set-Cookie"])
        return {"Cookie": f"{COOKIE}={cookie[COOKIE].value}"}


def accounts_ready(container):
    return subprocess.run(
        ["docker", "exec", container, "/usr/local/bin/api", "coordinator", "check"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=15,
    ).returncode == 0


def check(container):
    with request("/health/ready"):
        pass
    accounts, key = coordinator_config(container)
    required, mode = session_status()
    if accounts:
        return required and mode == "accounts" and anonymous_access_denied() and accounts_ready(container)
    expected_mode = "shared-key" if key is not None else "none"
    if required != (key is not None) or mode not in (None, expected_mode):
        return False
    if key is not None and not anonymous_access_denied():
        return False
    headers = session_cookie(key) if key is not None else {}
    try:
        with request(SNAPSHOT, headers=headers) as response:
            snapshot = json.load(response)
        return isinstance(snapshot["items"], list) and snapshot["source"]["status"] == "ok"
    finally:
        if headers:
            with request(SESSION, method="DELETE", headers=headers):
                pass


if __name__ == "__main__":
    try:
        sys.exit(0 if check(sys.argv[1]) else 1)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
        sys.exit(1)
