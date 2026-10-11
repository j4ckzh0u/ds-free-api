#!/usr/bin/env python3
"""OpenAI Responses API 端到端测试 —— 验证 POST /v1/responses

覆盖：
  1. 非流式：字符串 input
  2. 非流式：输入项数组（message + function_call + function_call_output）
  3. 流式：事件序列与 `data: [DONE]` 终止符
  4. 工具调用：function_call 输出项
  5. previous_response_id 多轮上下文

用法：
  uv run python test_responses.py
  uv run python test_responses.py --model deepseek-default
  uv run python test_responses.py --show-output
"""

import argparse
import json
import sys
import time
from datetime import datetime

# pyproject 已从 httpx 更名为 httpx2（API 同名），此处兼容别名
import httpx2 as httpx

from config import load_config

# Responses 流式事件必须包含的最小集合（顺序敏感）
EXPECTED_STREAM_EVENTS = [
    "response.created",
    "response.in_progress",
    "response.output_item.added",
    "response.content_part.added",
    "response.output_text.delta",
    "response.output_text.done",
    "response.content_part.done",
    "response.output_item.done",
    "response.completed",
]

WEATHER_TOOL = {
    "type": "function",
    "name": "get_weather",
    "description": "查询指定城市的天气",
    "parameters": {
        "type": "object",
        "properties": {"city": {"type": "string", "description": "城市名"}},
        "required": ["city"],
    },
}


class Client:
    def __init__(self, port: int, api_key: str):
        self.base = f"http://127.0.0.1:{port}"
        self.api_key = api_key
        self.http = httpx.Client(timeout=180)

    def headers(self) -> dict:
        return {
            "Content-Type": "application/json",
            "Authorization": f"Bearer {self.api_key}",
        }

    def post(self, path: str, payload: dict) -> httpx.Response:
        return self.http.post(f"{self.base}{path}", json=payload, headers=self.headers())

    def stream_events(self, payload: dict) -> tuple[list[dict], bool]:
        """消费 SSE，返回 (事件列表, 是否收到 [DONE])"""
        events: list[dict] = []
        saw_done = False
        with self.http.stream(
            "POST", f"{self.base}/v1/responses", json=payload, headers=self.headers()
        ) as resp:
            resp.raise_for_status()
            event_name = None
            for line in resp.iter_lines():
                if line.startswith("event: "):
                    event_name = line[len("event: ") :].strip()
                elif line.startswith("data: "):
                    data = line[len("data: ") :].strip()
                    if data == "[DONE]":
                        saw_done = True
                        continue
                    parsed = json.loads(data)
                    parsed.setdefault("type", event_name)
                    events.append(parsed)
                elif line == "":
                    event_name = None
        return events, saw_done


def check(name: str, condition: bool, detail: str = "") -> str | None:
    if condition:
        return None
    return f"{name} 失败{': ' + detail if detail else ''}"


def case_non_streaming_string(client: Client, model: str) -> dict:
    payload = {"model": model, "input": "只回复两个字：你好", "max_output_tokens": 64}
    resp = client.post("/v1/responses", payload)
    resp.raise_for_status()
    body = resp.json()

    errors = [
        check("object", body.get("object") == "response"),
        check("id_prefix", str(body.get("id", "")).startswith("resp_")),
        check("status", body.get("status") in ("completed", "incomplete"), str(body.get("status"))),
        check("output_non_empty", bool(body.get("output"))),
        check("usage_total", isinstance(body.get("usage", {}).get("total_tokens"), int)),
        check(
            "usage_field_names",
            "input_tokens" in body.get("usage", {}) and "output_tokens" in body.get("usage", {}),
        ),
        check("input_tokens_details", "cached_tokens" in body.get("usage", {}).get("input_tokens_details", {})),
        check(
            "output_tokens_details",
            "reasoning_tokens" in body.get("usage", {}).get("output_tokens_details", {}),
        ),
    ]
    output_types = [item.get("type") for item in body.get("output", [])]
    errors.append(check("output_types_valid", all(t in ("message", "reasoning", "function_call") for t in output_types), str(output_types)))

    text = extract_output_text(body)
    errors.append(check("text_non_empty", bool(text.strip())))

    return result("非流式-字符串 input", errors, text, body)


def case_non_streaming_items(client: Client, model: str) -> dict:
    payload = {
        "model": model,
        "instructions": "你是天气助手",
        "input": [
            {"role": "user", "content": "北京天气如何？"},
            {"role": "assistant", "content": [{"type": "output_text", "text": "我查一下"}]},
            {"role": "user", "content": [{"type": "input_text", "text": "谢谢"}]},
        ],
        "max_output_tokens": 64,
    }
    resp = client.post("/v1/responses", payload)
    resp.raise_for_status()
    body = resp.json()

    errors = [
        check("instructions_echo", body.get("instructions") == "你是天气助手", str(body.get("instructions"))),
        check("output_non_empty", bool(body.get("output"))),
    ]
    return result("非流式-输入项数组", errors, extract_output_text(body), body)


def case_streaming(client: Client, model: str) -> dict:
    payload = {"model": model, "input": "数到三", "stream": True, "max_output_tokens": 64}
    events, saw_done = client.stream_events(payload)

    types = [e["type"] for e in events]
    errors = [check("stream_terminated", saw_done, "缺少 data: [DONE]")]
    for expected in EXPECTED_STREAM_EVENTS:
        errors.append(check(f"event:{expected}", expected in types, f"实际事件: {types}"))

    errors.append(check("sequence_numbers", all(isinstance(e.get("sequence_number"), int) for e in events), "sequence_number 缺失"))
    seqs = [e["sequence_number"] for e in events if isinstance(e.get("sequence_number"), int)]
    errors.append(check("sequence_monotonic", seqs == sorted(seqs) and len(set(seqs)) == len(seqs)))

    deltas = [e for e in events if e["type"] == "response.output_text.delta"]
    errors.append(check("has_text_delta", bool(deltas)))
    errors.append(check("delta_has_item_id", all(e.get("item_id") for e in deltas)))
    errors.append(check("delta_has_content_index", all("content_index" in e for e in deltas)))

    completed = [e for e in events if e["type"] == "response.completed"]
    errors.append(check("single_completed", len(completed) == 1, f"got {len(completed)}"))
    if completed:
        resp = completed[0].get("response", {})
        errors.append(check("completed_status", resp.get("status") == "completed", str(resp.get("status"))))
        errors.append(check("completed_usage", resp.get("usage") is not None))
        errors.append(check("completed_output", bool(resp.get("output"))))

    text = "".join(e.get("delta", "") for e in deltas)
    return result("流式-事件序列", errors, text, {"events": types})


def case_tool_call(client: Client, model: str) -> dict:
    payload = {
        "model": model,
        "input": "北京天气怎么样？请调用工具查询。",
        "tools": [WEATHER_TOOL],
        "tool_choice": "auto",
        "max_output_tokens": 256,
    }
    resp = client.post("/v1/responses", payload)
    resp.raise_for_status()
    body = resp.json()

    calls = [i for i in body.get("output", []) if i.get("type") == "function_call"]
    errors = [check("has_function_call", bool(calls))]
    if calls:
        call = calls[0]
        errors.append(check("call_id", bool(call.get("call_id"))))
        errors.append(check("name", call.get("name") == "get_weather", str(call.get("name"))))
        errors.append(check("arguments_is_string", isinstance(call.get("arguments"), str)))
        errors.append(check("status", call.get("status") == "completed", str(call.get("status"))))
    return result("非流式-工具调用", errors, json.dumps(calls, ensure_ascii=False)[:200], body)


def case_streaming_tool_call(client: Client, model: str) -> dict:
    payload = {
        "model": model,
        "input": "北京天气怎么样？请调用工具查询。",
        "tools": [WEATHER_TOOL],
        "stream": True,
        "max_output_tokens": 256,
    }
    events, saw_done = client.stream_events(payload)
    types = [e["type"] for e in events]

    errors = [check("stream_terminated", saw_done)]
    errors.append(check("has_args_delta", "response.function_call_arguments.delta" in types, str(types)))
    errors.append(check("has_args_done", "response.function_call_arguments.done" in types, str(types)))

    completed = [e for e in events if e["type"] == "response.completed"]
    if completed:
        calls = [i for i in completed[0]["response"].get("output", []) if i.get("type") == "function_call"]
        errors.append(check("completed_has_call", bool(calls)))
    else:
        errors.append("缺少 response.completed")

    return result("流式-工具调用", errors, "", {"events": types})


def case_previous_response_id(client: Client, model: str) -> dict:
    first = client.post(
        "/v1/responses",
        {"model": model, "input": "记住数字 42，只回复 OK", "max_output_tokens": 32},
    )
    first.raise_for_status()
    first_body = first.json()
    first_id = first_body["id"]

    second = client.post(
        "/v1/responses",
        {"model": model, "input": "我刚才让你记住的数字是多少？", "previous_response_id": first_id, "max_output_tokens": 64},
    )
    second.raise_for_status()
    second_body = second.json()

    errors = [
        check("first_id_prefix", first_id.startswith("resp_")),
        check("previous_response_id_echo", second_body.get("previous_response_id") == first_id, str(second_body.get("previous_response_id"))),
    ]
    text = extract_output_text(second_body)
    errors.append(check("second_response_non_empty", bool(text.strip())))
    # 上下文是否真的带过去：回复里应出现 42
    errors.append(check("context_carried", "42" in text, f"回复未包含 42: {text[:120]}"))

    # 未知 ID 必须是 400 而不是 500
    bad = client.post(
        "/v1/responses",
        {"model": model, "input": "hi", "previous_response_id": "resp_does_not_exist"},
    )
    errors.append(check("unknown_previous_id_400", bad.status_code == 400, f"实际 {bad.status_code}"))

    return result("多轮-previous_response_id", errors, text, second_body)


def case_error_shapes(client: Client, model: str) -> dict:
    bad_model = client.post("/v1/responses", {"model": "no-such-model", "input": "hi"})
    errors = [
        check("bad_model_400", bad_model.status_code == 400, f"实际 {bad_model.status_code}"),
        check(
            "bad_model_envelope",
            bad_model.json().get("error", {}).get("type") == "invalid_request_error",
            json.dumps(bad_model.json(), ensure_ascii=False)[:160],
        ),
    ]

    # 缺少 model 字段
    no_model = client.post("/v1/responses", {"input": "hi"})
    errors.append(check("missing_model_400", no_model.status_code == 400, f"实际 {no_model.status_code}"))

    # 缺少 input
    no_input = client.post("/v1/responses", {"model": model})
    errors.append(check("missing_input_400", no_input.status_code == 400, f"实际 {no_input.status_code}"))

    return result("错误信封", errors, "", {})


def extract_output_text(body: dict) -> str:
    parts = []
    for item in body.get("output", []):
        if item.get("type") != "message":
            continue
        for content in item.get("content", []):
            if content.get("type") == "output_text":
                parts.append(content.get("text", ""))
    return "".join(parts)


def result(name: str, errors: list[str | None], text: str, raw: object) -> dict:
    real_errors = [e for e in errors if e]
    return {
        "name": name,
        "passed": not real_errors,
        "errors": real_errors,
        "text": text,
        "raw": raw,
    }


def main() -> int:
    config = load_config()

    parser = argparse.ArgumentParser(description="OpenAI Responses API e2e 测试")
    parser.add_argument("--model", type=str, default=None)
    parser.add_argument("--show-output", action="store_true")
    parser.add_argument("--report", type=str, default=None)
    args = parser.parse_args()

    model = args.model or config["models"][0]
    client = Client(config["port"], config["api_key"])

    cases = [
        ("非流式-字符串 input", case_non_streaming_string),
        ("非流式-输入项数组", case_non_streaming_items),
        ("流式-事件序列", case_streaming),
        ("非流式-工具调用", case_tool_call),
        ("流式-工具调用", case_streaming_tool_call),
        ("多轮-previous_response_id", case_previous_response_id),
        ("错误信封", case_error_shapes),
    ]

    print(f"\nResponses API 测试")
    print(f"  模型: {model}  端口: {config['port']}  用例: {len(cases)}")

    results = []
    for label, fn in cases:
        start = time.time()
        try:
            r = fn(client, model)
        except Exception as e:  # noqa: BLE001 - e2e 需要把任何异常汇总进报告
            r = {"name": label, "passed": False, "errors": [f"异常: {e}"], "text": "", "raw": {}}
        r["duration"] = time.time() - start
        results.append(r)
        status = "✓" if r["passed"] else "✗"
        err = f" | {'; '.join(r['errors'])[:120]}" if r["errors"] else ""
        print(f"  {status} {label} | {r['duration']:.1f}s{err}")
        if args.show_output and r.get("text"):
            print(f"      └ {r['text'][:200]}")

    passed = sum(1 for r in results if r["passed"])
    print(f"\n{'=' * 60}")
    print(f"  总计: {len(results)}  |  通过: {passed}  |  失败: {len(results) - passed}")
    print(f"{'=' * 60}\n")

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump(
                {
                    "suite": "responses",
                    "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                    "model": model,
                    "summary": {"total": len(results), "passed": passed, "failed": len(results) - passed},
                    "results": results,
                },
                f,
                ensure_ascii=False,
                indent=2,
            )

    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
