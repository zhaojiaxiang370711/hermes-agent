#!/usr/bin/env python3
"""
boxingAgent 语音运行时 - 真实麦克风测试

用法：
    python3 voice_test.py              # 完整测试（唤醒 + ASR）
    python3 voice_test.py --asr-only   # 跳过唤醒，直接测试 ASR
    python3 voice_test.py --record 15  # 录 15 秒（默认 10）

依赖：pip install sounddevice numpy

前置条件：
    boxing-agent voice  # 启动语音运行时（sherpa-onnx 模式）
"""

import argparse
import base64
import json
import threading
import time
import urllib.request

import numpy as np
import sounddevice as sd

VOICE_URL = "http://127.0.0.1:50058"
MIC_DEVICE = next(
    (
        i
        for i, d in enumerate(sd.query_devices())
        if "ugreen" in d["name"].lower() and d["max_input_channels"] > 0
    ),
    None,
)
SAMPLE_RATE = 48000


def find_mic():
    if MIC_DEVICE is not None:
        return MIC_DEVICE
    for i, d in enumerate(sd.query_devices()):
        if d["max_input_channels"] > 0:
            return i
    return sd.default.device[0]


def reset_runtime():
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                f"{VOICE_URL}/api/v1/voice-runtime/reset", method="POST"
            ),
            timeout=5,
        )
        return True
    except Exception:
        return False


def simulate_wake():
    try:
        urllib.request.urlopen(
            urllib.request.Request(
                f"{VOICE_URL}/api/v1/voice-runtime/simulate/wake",
                data=json.dumps({"keyword": "小星", "confidence": 0.95}).encode(),
                headers={"Content-Type": "application/json"},
            ),
            timeout=5,
        )
        return True
    except Exception:
        return False


def record_audio(duration, mic_device):
    """录制麦克风音频，返回 numpy 数组（48kHz i16）"""
    audio = sd.rec(
        int(duration * SAMPLE_RATE),
        samplerate=SAMPLE_RATE,
        channels=1,
        dtype="int16",
        device=mic_device,
    )
    sd.wait()
    return audio


def resample_48_to_16(audio):
    """48kHz → 16kHz 简单下采样"""
    return audio.reshape(-1, 3).mean(axis=1).astype(np.int16)


def send_pcm_chunks(pcm_i16, sr=16000, chunk_ms=500):
    """分块发送 PCM 到语音运行时"""
    chunk_size = int(sr * chunk_ms / 1000)
    for i in range(0, len(pcm_i16), chunk_size):
        chunk = pcm_i16[i : i + chunk_size]
        if len(chunk) == 0:
            continue
        b64 = base64.b64encode(chunk.tobytes()).decode()
        try:
            urllib.request.urlopen(
                urllib.request.Request(
                    f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
                    data=json.dumps(
                        {"pcm16_base64": b64, "sample_rate": sr}
                    ).encode(),
                    headers={"Content-Type": "application/json"},
                ),
                timeout=10,
            )
        except Exception:
            pass
        time.sleep(0.02)

    # final_chunk
    urllib.request.urlopen(
        urllib.request.Request(
            f"{VOICE_URL}/api/v1/voice-runtime/audio/pcm",
            data=json.dumps(
                {"pcm16_base64": "", "sample_rate": sr, "final_chunk": True}
            ).encode(),
            headers={"Content-Type": "application/json"},
        ),
        timeout=10,
    )


def listen_events(duration):
    """监听 SSE 事件，返回事件列表"""
    events = []
    stop = threading.Event()

    def _listen():
        try:
            req = urllib.request.Request(f"{VOICE_URL}/api/v1/voice-runtime/events")
            resp = urllib.request.urlopen(req, timeout=duration + 5)
            for line in resp:
                if stop.is_set():
                    break
                line = line.decode().strip()
                if line.startswith("data:"):
                    ev = json.loads(line[5:].strip())
                    events.append(ev)
                    t = ev.get("type")
                    if t == "wake_word":
                        kw = ev.get("keyword", "?")
                        conf = ev.get("confidence", "?")
                        print(f"  🔔 唤醒词: '{kw}' (置信度 {conf})")
                    elif t == "transcript" and ev.get("final_result"):
                        print(f"  📝 转写: {ev.get('transcript')}")
        except Exception:
            pass

    thread = threading.Thread(target=_listen, daemon=True)
    thread.start()
    return events, stop, thread


def check_runtime():
    """检查语音运行时是否可用"""
    try:
        resp = urllib.request.urlopen(f"{VOICE_URL}/health", timeout=3)
        d = json.loads(resp.read())
        return d.get("healthy", False)
    except Exception:
        return False


def get_status():
    """获取运行时状态"""
    try:
        resp = urllib.request.urlopen(f"{VOICE_URL}/api/v1/voice-runtime/status", timeout=3)
        return json.loads(resp.read())
    except Exception:
        return None


def main():
    parser = argparse.ArgumentParser(description="boxingAgent 语音运行时测试")
    parser.add_argument("--asr-only", action="store_true", help="跳过唤醒词，直接测试 ASR")
    parser.add_argument("--record", type=int, default=10, help="录音秒数（默认 10）")
    args = parser.parse_args()

    # 检查运行时
    if not check_runtime():
        print("❌ 语音运行时未启动")
        print("   请先运行: boxing-agent voice")
        print("   或设置 QXZN_VOICE_RUNTIME_ENGINE=sherpa-onnx 后启动")
        return

    status = get_status()
    if status:
        engine = status.get("engine", "?")
        kw_model = status.get("kws_model_exists", False)
        asr_model = status.get("asr_model_configured", False)
        print(f"✅ 语音运行时已启动 (引擎: {engine})")
        print(f"   唤醒词模型: {'✅' if kw_model else '❌'}  ASR模型: {'✅' if asr_model else '❌'}")

    mic = find_mic()
    mic_name = sd.query_devices(mic)["name"]
    print(f"🎤 麦克风: {mic_name}")
    duration = args.record
    print(f"⏱️  录音 {duration} 秒\n")

    # 重置运行时
    reset_runtime()

    if args.asr_only:
        # 跳过唤醒，直接模拟唤醒后 ASR
        print("🔔 模拟唤醒（跳过唤醒词检测）...")
        simulate_wake()
    else:
        print("📢 请对着麦克风说「小星」，然后说一句话")
        print("   （如果唤醒词检测不到，会自动用模拟唤醒测试 ASR）\n")

    # 启动 SSE 监听
    events, stop, thread = listen_events(duration + 3)

    # 录音
    print("🔴 录音中...")
    audio = record_audio(duration, mic)
    print(f"✅ 录音完成 ({len(audio)} 样本 @ {SAMPLE_RATE}Hz)")

    # 重采样
    pcm = resample_48_to_16(audio)
    print(f"📡 重采样到 {len(pcm)} 样本 (16kHz)")

    # 发送
    print("📡 发送 PCM 到语音运行时...")
    send_pcm_chunks(pcm)

    # 等待事件
    time.sleep(2)
    stop.set()
    thread.join(timeout=1)

    # 汇总
    wakes = [e for e in events if e.get("type") == "wake_word"]
    transcripts = [e for e in events if e.get("type") == "transcript" and e.get("final_result")]

    if wakes and transcripts:
        print(f"\n✅ 完整流程成功！")
        print(f"   唤醒词: '{wakes[0].get('keyword')}' (置信度 {wakes[0].get('confidence')})")
        print(f"   转写: {transcripts[0].get('transcript')}")
    elif transcripts:
        print(f"\n✅ ASR 转写成功: {transcripts[0].get('transcript')}")
    elif not args.asr_only:
        print(f"\n⚠️  未检测到唤醒词（请说「小星」）")
        print(f"   尝试用 --asr-only 跳过唤醒词，直接测试 ASR：")
        print(f"   python3 voice_test.py --asr-only --record {duration}")


if __name__ == "__main__":
    main()
