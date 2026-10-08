"""Collide for Hermes: the same hooks Claude Code, Codex and Cursor run, plus
teammates' messages delivered into the open conversation.

A thin shim. Every decision is made by the `collide` program
(~/.collide/bin/collide), exactly as for the other agents; this file only
hands Hermes's hook calls to it and its answers back. Installed and kept up
to date by `collide install`; removed by `collide uninstall`.
"""

import json
import os
import subprocess
import threading
from pathlib import Path

_listeners = {}
_briefed = set()
_lock = threading.Lock()


def _program():
    home = os.environ.get("COLLIDE_HOME") or str(Path.home())
    name = "collide.exe" if os.name == "nt" else "collide"
    path = Path(home) / ".collide" / "bin" / name
    return str(path) if path.exists() else ""


def _cwd():
    try:
        from agent.runtime_cwd import resolve_agent_cwd
        return str(resolve_agent_cwd())
    except Exception:
        return os.getcwd()


def _call(command, event, kwargs, timeout=30):
    program = _program()
    if not program:
        return {}
    args = kwargs.get("args")
    payload = {
        "hook_event_name": event,
        "tool_name": kwargs.get("tool_name"),
        "tool_input": args if isinstance(args, dict) else None,
        "tool_response": kwargs.get("result") if isinstance(kwargs.get("result"), str) else None,
        "session_id": kwargs.get("session_id") or "",
        "cwd": _cwd(),
        "prompt": kwargs.get("user_message") or "",
        "model": kwargs.get("model") or "",
    }
    try:
        done = subprocess.run(
            [program, command, "--harness", "hermes"], input=json.dumps(payload, default=str),
            capture_output=True, text=True, timeout=timeout,
            env=dict(os.environ, COLLIDE_MACHINE="1"),
        )
        out = (done.stdout or "").strip()
        return json.loads(out) if out.startswith("{") else {}
    except Exception:
        return {}


def _listen(ctx, session_id):
    """One `collide listen` per session: each line it prints is a batch of
    teammates' messages, injected into this conversation."""
    program = _program()
    if not program or not session_id:
        return
    with _lock:
        if session_id in _listeners and _listeners[session_id].poll() is None:
            return
        try:
            child = subprocess.Popen(
                [program, "listen", session_id, "--print"], cwd=_cwd(), stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            )
        except Exception:
            return
        _listeners[session_id] = child

    def pump():
        for line in child.stdout:
            try:
                text = json.loads(line).get("text", "")
            except Exception:
                continue
            if text:
                ctx.inject_message(text, role="user", session_key=session_id)

    threading.Thread(target=pump, daemon=True, name=f"collide-listen-{session_id[:8]}").start()


def register(ctx):
    def pre_llm_call(session_id="", is_first_turn=False, **kwargs):
        kwargs["session_id"] = session_id
        said = []
        with _lock:
            first = session_id not in _briefed
            _briefed.add(session_id)
        if first:
            said.append(_call("report", "on_session_start", kwargs).get("context", ""))
            _listen(ctx, session_id)
        said.append(_call("report", "pre_llm_call", kwargs).get("context", ""))
        said = [s for s in said if s]
        return {"context": "\n\n".join(said)} if said else None

    def pre_tool_call(**kwargs):
        answer = _call("gate", "pre_tool_call", kwargs, timeout=10)
        if answer.get("action") == "block":
            return {"action": "block", "message": answer.get("message", "")}
        return None

    def post_tool_call(**kwargs):
        threading.Thread(target=_call, args=("report", "post_tool_call", kwargs), daemon=True).start()

    def on_session_end(**kwargs):
        threading.Thread(target=_call, args=("report", "on_session_end", kwargs), daemon=True).start()

    def on_session_finalize(session_id="", **kwargs):
        with _lock:
            child = _listeners.pop(session_id, None)
        if child is not None and child.poll() is None:
            child.terminate()

    ctx.register_hook("pre_llm_call", pre_llm_call)
    ctx.register_hook("pre_tool_call", pre_tool_call)
    ctx.register_hook("post_tool_call", post_tool_call)
    ctx.register_hook("on_session_end", on_session_end)
    ctx.register_hook("on_session_finalize", on_session_finalize)
