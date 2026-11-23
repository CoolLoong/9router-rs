import argparse
from html import escape
import json
import math
from pathlib import Path


METRICS = (
    ("Throughput", "req/s", ("successful_requests_per_second",), 1, 1, "Higher is better"),
    ("p95 latency", "ms", ("latency_ms", "p95"), 1, 2, "Lower is better"),
    ("p50 latency", "ms", ("latency_ms", "p50"), 1, 2, "Lower is better"),
    ("p99 latency", "ms", ("latency_ms", "p99"), 1, 2, "Lower is better"),
    ("Gateway CPU / request", "ms", ("cpu_ms_per_request",), 1, 2, "Lower is better"),
    ("Sampled peak RSS", "MiB", ("sampled_peak_rss_kib",), 1024, 1, "Lower is better"),
    ("Warm RSS", "MiB", ("warm_rss_kib",), 1024, 1, "Lower is better"),
    ("Time to ready", "ms", ("cold_ready_ms",), 1, 1, "Lower is better"),
)
SERIES = (("node", "Original Node.js", "#486581"), ("rust", "9Router Rust", "#b94721"))


def measurement(summary, path, divisor):
    for key in path:
        summary = summary[key]
    values = tuple(float(summary[key]) / divisor for key in ("min", "median", "max"))
    if not (all(math.isfinite(value) for value in values) and 0 <= values[0] <= values[1] <= values[2]):
        raise ValueError(f"Invalid benchmark range for {'.'.join(path)}")
    return values


def text(x, y, value, kind=""):
    style = f' class="{kind}"' if kind else ""
    return f'<text x="{x}" y="{y}"{style}>{escape(str(value))}</text>'


def render(report):
    if report.get("schema_version") != 1:
        raise ValueError("Expected benchmark JSON schema version 1")
    options = report["options"]
    summary = report["summaries"]
    rounds = options["rounds"]
    for key, _, _ in SERIES:
        if summary[key] is None or summary[key]["valid_rounds"] != rounds:
            raise ValueError(f"{key} does not have {rounds} valid benchmark rounds")

    provenance = report.get("provenance", {})
    version = provenance.get("rust_base_commit", "")[:8]
    scope = f"{rounds} rounds per gateway · {options['requests']:,} requests per round · concurrency {options['concurrency']}"
    if version:
        scope += f" · Rust {version}"

    elements = []
    descriptions = []
    for index, (label, unit, path, divisor, decimals, direction) in enumerate(METRICS):
        x = 26 + index % 2 * 474
        y = 134 + index // 2 * 139
        ranges = [measurement(summary[key], path, divisor) for key, _, _ in SERIES]
        scale = max(item[2] for item in ranges)
        scale = scale if scale > 0 else 1
        elements += [
            f'<rect x="{x}" y="{y}" width="448" height="126" rx="10" fill="#fff" stroke="#e2e8f0"/>',
            text(x + 18, y + 27, label, "panel-title"),
            text(x + 289, y + 27, f"{unit} · {direction}", "caption"),
        ]
        for row, ((_, name, color), (minimum, median, maximum)) in enumerate(zip(SERIES, ranges)):
            bar_y = y + 48 + row * 33
            left = x + 18
            width = 303
            percent = lambda value: width * value / scale
            elements += [
                f'<rect x="{left}" y="{bar_y}" width="{width}" height="14" rx="4" fill="#f1f5f9"/>',
                f'<rect x="{left}" y="{bar_y}" width="{percent(median):.2f}" height="14" rx="4" fill="{color}"/>',
                f'<path d="M {left + percent(minimum):.2f},{bar_y + 18} H {left + percent(maximum):.2f} '
                f'M {left + percent(minimum):.2f},{bar_y + 15} v 6 '
                f'M {left + percent(maximum):.2f},{bar_y + 15} v 6" stroke="{color}" stroke-width="1.2" fill="none"/>',
                text(x + 337, bar_y + 12, f"{median:,.{decimals}f} {unit}", "value"),
            ]
            descriptions.append(f"{label}, {name}, median {median:,.{decimals}f} {unit}.")
    desc = escape(" ".join(descriptions))
    rows = "\n".join(elements)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="974" height="716" viewBox="0 0 974 716" role="img" aria-labelledby="title description">
<title id="title">9Router gateway benchmark</title>
<desc id="description">{desc}</desc>
<style>
text {{ font-family: Inter, system-ui, sans-serif; fill: #172b44; }}
.title {{ font-size: 29px; font-weight: 720; }}
.panel-title {{ font-size: 16px; font-weight: 680; }}
.caption {{ font-size: 12px; fill: #526377; }}
.value {{ font-size: 13px; font-weight: 650; }}
</style>
<rect width="974" height="716" fill="#f8fafc"/>
{text(26, 47, "9Router gateway benchmark", "title")}
{text(27, 76, scope, "caption")}
<rect x="27" y="98" width="13" height="13" rx="3" fill="#486581"/>
{text(46, 109, "Original Node.js", "caption")}
<rect x="164" y="98" width="13" height="13" rx="3" fill="#b94721"/>
{text(183, 109, "9Router Rust", "caption")}
{text(306, 109, "Bars show medians. Whiskers show the range across rounds.", "caption")}
{rows}
{text(27, 700, "Linux loopback · non-streaming Chat Completions · same echo upstream", "caption")}
</svg>
"""


def main():
    parser = argparse.ArgumentParser(description="Draw an SVG comparison from benchmark JSON.")
    parser.add_argument("report", type=Path, help="JSON from nine-router-benchmark")
    parser.add_argument("output", type=Path, help="Destination SVG path")
    args = parser.parse_args()
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        image = render(report)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(image, encoding="utf-8")
    except (OSError, KeyError, TypeError, ValueError) as error:
        parser.exit(1, f"Could not plot benchmark: {error}\n")


if __name__ == "__main__":
    main()
