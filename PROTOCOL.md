# 网桥 FetchBridge 协议规范 (v3)

使用 Claude Opus 4.7 xhigh 编写，可能存在细微错误，请视实际情况使用

本协议描述 AstroBox NG 插件 **网桥 FetchBridge** 与运行在小米穿戴设备上的快应用之间，
通过 QAIC `interconnect` 通道交换 HTTP 请求/响应所使用的消息格式。

---

## 1. 协议版本

| 版本 | 状态     | 引入内容                                                                                  |
| ---- | -------- | ----------------------------------------------------------------------------------------- |
| v1   | 永久兼容 | 握手计数 ping-pong；fetch 单消息响应（文本直传 或 base64）。                              |
| v2   | 永久兼容 | 在 v1 基础上，握手包加入 `caps` 协商；新增 `fetch-chunk` 分片响应路径。                   |
| v3   | 当前     | 在 v2 基础上，握手 `caps` 加入 `encodings` / `compressions` 数组，可协商编码与压缩方式。 |

**兼容承诺**：

- v3 插件对未带 `caps` 或仅声明 v1/v2 能力的快应用，行为与对应版本**完全一致**。
- v3 插件对 v3 快应用，按双方共同支持的能力交集 + 对端偏好顺序选择编码/压缩。
- 任何 v3 快应用都必须保留对 v1 单消息 `base64`/`text` 响应的处理能力——
  这是首响应到达前协商尚未完成时的兜底路径。

---

## 2. 传输层

- 双方通过 QAIC `interconnect` 通道交换 **UTF-8 字符串**，内容是一段 JSON 文本。
- 一条 JSON 对象 = 一帧逻辑消息，包含一个 `tag` 字段标识用途，其它字段由 `tag` 决定。
- 主机会把同 (设备地址 `addr`, 快应用包名 `pkg`) 的消息归到同一会话。

> 注意：JSON 字符串大小、单帧字节长度受底层 QAIC 通道限制约束，这正是 v2 引入分片、
> v3 引入压缩的原因——前者用于突破单帧上限，后者用于在带宽受限的 BLE 场景下省传输时间。

### 2.1 消息标签 (tag)

| tag           | 方向                                              | 说明                                       |
| ------------- | ------------------------------------------------- | ------------------------------------------ |
| `__hs__`      | 双向                                              | 握手与能力协商。                           |
| `fetch`       | 快应用 → 插件 (请求) / 插件 → 快应用 (响应或响应头) | HTTP 调用本体；当响应分片时仅承载头部元信息。 |
| `fetch-chunk` | 插件 → 快应用                                     | 分片模式下携带响应体的某一分片。             |

未知 `tag` 应当被对端忽略（仅记日志），不得报错断开会话。

---

## 3. 握手协议

### 3.1 时序

```
QuickApp                              FetchBridge
   |                                       |
   |-- {tag:"__hs__", count:0, caps?} ---->|
   |                                       |  state: open=true, store peer caps
   |<-- {tag:"__hs__", count:1, caps} -----|
   |                                       |
   |-- {tag:"__hs__", count:2, caps} ----->|
   |                                       |  count>=2: 停止 ping-pong
```

任何一方收到 `count<2` 时回送 `count+1`；`count>=2` 视为握手完成，不再回包。

### 3.2 `__hs__` 包字段

| 字段    | 类型           | 必填   | 说明                                                |
| ------- | -------------- | ------ | --------------------------------------------------- |
| `tag`   | string         | 是     | 固定为 `"__hs__"`。                                  |
| `count` | integer        | 是     | 计数器，范围 `[0,2]`。                               |
| `caps`  | object \| null | v1 否 / v2+ 推荐 | 能力声明，缺省视为 v1 客户端（不分片、不压缩、走 v1 编码）。 |

### 3.3 `caps` 对象

| 字段           | 类型              | 缺省       | 说明                                                                                  |
| -------------- | ----------------- | ---------- | ------------------------------------------------------------------------------------- |
| `version`      | integer           | `1`        | 本端支持的最高协议版本。                                                              |
| `chunk`        | boolean           | `false`    | 本端是否支持收发分片 fetch 响应。                                                     |
| `maxChunkSize` | integer           | 服务端默认 | 本端可处理的单分片**编码前字节数**上限。                                              |
| `encodings`    | array\<string\>   | `[]`       | 本端可**解码**的 wire 编码集合，**按偏好顺序排列**（第一个为最优先）。              |
| `compressions` | array\<string\>   | `[]`       | 本端可**解压**的压缩算法集合，**按偏好顺序排列**。                                    |

`encodings` 取值（详见 §6）：

| 值       | 含义                                                                |
| -------- | ------------------------------------------------------------------- |
| `text`   | JSON 字符串直接承载 UTF-8 文本字节，无需解码。仅适用于非分片、非压缩、可被 UTF-8 解析的响应。 |
| `base64` | 标准 base64（RFC 4648）。膨胀 ~4/3，解码中等开销。**v1/v2 基线，任何端都必须支持**。 |
| `hex`    | 小写十六进制（`0-9a-f`）。膨胀 2×，解码极简——每字节两次查表，适合 RTOS。 |

`compressions` 取值：

| 值        | 含义                                                                                          |
| --------- | --------------------------------------------------------------------------------------------- |
| `none`    | 不压缩。**任何端都隐式支持**，可省略不写。                                                    |
| `deflate` | 原始 deflate (RFC 1951，无 zlib 头)。压缩率最好，解压相对吃 CPU。JS 端常用 `pako`/`fflate`。 |
| `lz4`     | LZ4 块格式 (frame 之外的裸 block)。压缩率不如 deflate，但解压极快、内存占用小，更适合 MCU。   |

### 3.4 协商规则

设 `peer` 为对端声明，`local` 为本端配置：

```
negotiated.version       = min(peer.version, local.version)
negotiated.chunked       = local.chunk && peer.chunk && version >= 2
negotiated.chunkSize     = clamp(min(peer.maxChunkSize || local.maxChunkSize,
                                      local.maxChunkSize),
                                 MIN_CHUNK_SIZE, local.maxChunkSize)
negotiated.encodings     = peer.encodings ∩ local.encodings   // 保留 peer 顺序
negotiated.compressions  = peer.compressions ∩ local.compressions
```

**单次响应的编码/压缩选择**（由发送方按本端策略 + 对端偏好顺序决定）：

1. **压缩**：从 `negotiated.compressions` 取第一个本端支持的；如果对端没声明，
   或响应体太小（< 256 字节），用 `none`。
2. **是否分片**：在压缩之后判定。当 `negotiated.chunked && payload.length > chunkSize` 时分片。
3. **编码**：
   - 若**未压缩**、**未分片**、payload 是合法 UTF-8、对端 `encodings` 含 `text` ⇒ `text`。
   - 否则按 `negotiated.encodings` 顺序选第一个 `base64` 或 `hex`。
   - 都没有时回落到 `base64`（v1/v2 baseline）。

**插件当前默认值**：

```
LOCAL_PROTOCOL_VERSION   = 3
LOCAL_CHUNK_SUPPORTED    = true
LOCAL_MAX_CHUNK_SIZE     = 4096   bytes (压缩后)
MIN_CHUNK_SIZE           = 256    bytes
COMPRESS_MIN_SIZE        = 256    bytes  // 小于此阈值不压缩
LOCAL_ENCODINGS          = ["base64", "hex", "text"]
LOCAL_COMPRESSIONS       = ["none", "deflate", "lz4"]
```

- 任意一端未声明 `caps` ⇒ 全部走 v1 路径（单消息、`text`/`base64`、不压缩）。
- 任意一端 `chunk=false` ⇒ 不分片。
- `version<2` ⇒ 不分片。
- `encodings`/`compressions` 缺失或交集为空 ⇒ 走 v1/v2 baseline 默认值。

---

## 4. Fetch 请求 (QuickApp → FetchBridge)

```json
{
  "tag": "fetch",
  "id":  "<可选请求 id>",
  "url": "https://example.com/api",
  "options": {
    "method":  "GET",
    "headers": { "Accept": "application/json" },
    "body":    "<请求体字符串>",
    "raw":     false
  }
}
```

| 字段                | 类型              | 必填 | 说明                                                       |
| ------------------- | ----------------- | ---- | ---------------------------------------------------------- |
| `id`                | string            | 否   | 请求关联 id；响应与分片会原样回带此字段以便多路复用。       |
| `url`               | string            | 是   | 目标 URL。                                                  |
| `options.method`    | string            | 否   | HTTP 方法，缺省 `GET`，不区分大小写。                       |
| `options.headers`   | object            | 否   | 请求头键值表，值为字符串；非字符串会被 `toString` 化。       |
| `options.body`      | string            | 否   | 请求体。当前只支持字符串；二进制请发 base64 自行约定编码。 |
| `options.raw`       | boolean           | 否   | `true` 表示要求响应体按字节返回（不做 UTF-8 解码）；缺省 `false`。 |

> v3 当前**未对请求体引入压缩/分片**——上行请求通常很小、且来自更弱的设备。
> 若未来扩展，将在 `options` 里加 `bodyEncoding` / `bodyCompression` 字段，同样向后兼容。

---

## 5. Fetch 响应 (FetchBridge → QuickApp)

不论是否分片、是否压缩，**头部消息**永远使用 `tag:"fetch"`，并保持与 v1 一致的六个核心字段。
新增字段都是**可选**的；缺省时行为退回到 v1。

### 5.1 单消息模式

```json
{
  "tag": "fetch",
  "id":  "<原 id>",
  "resp": {
    "ok":         true,
    "status":     200,
    "statusText": "OK",
    "headers":    { "content-type": "application/json" },
    "body":       "<编码后的字符串>",
    "raw":        false,

    "bodyEncoding": "hex",      // 可选；缺省按 raw 推断 (raw=false⇒text, raw=true⇒base64)
    "compression":  "deflate",  // 可选；缺省 "none"
    "originalBytes": 12480      // 可选；仅当 compression != "none" 时出现，单位字节
  }
}
```

| 字段             | 解释                                                                                     |
| ---------------- | ---------------------------------------------------------------------------------------- |
| `body`           | 经 `compression` → `bodyEncoding` 处理后的最终字符串。                                  |
| `raw`            | 解压、解码后字节的解释方式：`true`=二进制，`false`=按 UTF-8 解码为文本。                 |
| `bodyEncoding`   | `text` / `base64` / `hex`。缺省按 v1 规则推断（见上）。                                  |
| `compression`    | `none` / `deflate` / `lz4`。缺省为 `none`。                                              |
| `originalBytes`  | 原始字节数（解压后），用于预分配缓冲；省略时与解码后的 `body` 长度相等。                 |

**解码顺序（接收端）**：

```
encoded string  --(bodyEncoding decode)-->  payload bytes
payload bytes   --(compression decompress)-->  original bytes
original bytes  --(raw ? keep : UTF-8 decode)--> 最终 body
```

### 5.2 分片模式

**第 1 帧 — 头部**（同样使用 `tag:"fetch"`）：

```json
{
  "tag": "fetch",
  "id":  "<原 id>",
  "resp": {
    "ok":         true,
    "status":     200,
    "statusText": "OK",
    "headers":    { "...": "..." },
    "body":       "",
    "raw":        true,

    "chunked":     true,
    "totalBytes":  20480,        // 编码前、压缩后字节数 = 所有 chunk 解码后长度之和
    "chunkSize":   4096,
    "chunkCount":  5,

    "bodyEncoding": "base64",    // 分片必然是 text 之外的二进制编码
    "compression":  "lz4",       // 可选；缺省 "none"
    "originalBytes": 65536       // 可选；解压后字节数
  }
}
```

**第 2..N 帧 — 分片**：

```json
{
  "tag":  "fetch-chunk",
  "id":   "<原 id>",
  "seq":  0,
  "total": 5,
  "data": "<bodyEncoding 编码后的本分片字符串>"
}
```

| 字段              | 说明                                                          |
| ----------------- | ------------------------------------------------------------- |
| `resp.chunked`    | 固定 `true`，标识分片模式。                                  |
| `resp.totalBytes` | 所有 chunk **解码后**长度之和（= 压缩后字节数）。              |
| `resp.chunkSize`  | 单个分片承载的解码后字节数（最后一片可能更小）。               |
| `resp.chunkCount` | 分片总数。                                                    |
| `resp.bodyEncoding` | 分片场景一定是 `base64` 或 `hex`，决不会是 `text`。           |
| `resp.compression` | 压缩算法；与单消息模式语义一致。                              |
| `resp.originalBytes` | 解压后字节数；仅 `compression != "none"` 时出现。            |
| chunk `seq`       | 分片序号，`0..chunkCount-1`。                                  |
| chunk `total`     | 冗余校验值，应与头部的 `chunkCount` 相等。                      |
| chunk `data`      | 本分片的 `bodyEncoding` 编码。解码后长度应为 `chunkSize`，最后一片可能更短。 |

**重组规则**：

1. 收到 `resp.chunked === true` 时，按 `id` 建立缓冲区，记录 `chunkCount/totalBytes/raw/bodyEncoding/compression/originalBytes`。
2. 每收到一个 `fetch-chunk`：按 `bodyEncoding` 解码 → 写入 `buffer[seq]`。
3. 全部 `chunkCount` 个分片到齐：拼接缓冲区，总长度应等于 `totalBytes`。
4. 若 `compression != "none"`：按算法解压缩，结果长度应等于 `originalBytes`（若提供）。
5. 若 `raw === false`：以 UTF-8 解码为字符串；否则保留为字节。

### 5.3 错误响应

发生网络/送达失败时，永远使用单消息 v1 形态，不带任何 v2/v3 元信息：

```json
{
  "tag": "fetch",
  "id":  "<原 id>",
  "resp": {
    "ok":         false,
    "status":     0,
    "statusText": "<错误描述>",
    "headers":    {},
    "body":       "",
    "raw":        false
  }
}
```

---

## 6. 编码与压缩方式参考

### 6.1 编码 `bodyEncoding`

| 编码     | 膨胀率 | 解码复杂度       | 适合场景                                        |
| -------- | ------ | ---------------- | ----------------------------------------------- |
| `text`   | 1.0×   | 零                | 文本 API（JSON、HTML）且未压缩、未分片。        |
| `base64` | ~1.33× | 查表 + 位移      | 通用二进制（兼容性最好，v1/v2 基线）。           |
| `hex`    | 2.0×   | 两次查表（极简） | RTOS / 低算力 MCU，宁愿多传一倍字节也要降解码开销。 |

### 6.2 压缩 `compression`

| 算法      | 压缩率           | 编码 CPU | 解码 CPU       | 内存占用 | 适合场景                                       |
| --------- | ---------------- | -------- | -------------- | -------- | ---------------------------------------------- |
| `none`    | 1.0              | 零       | 零             | 零       | 短响应、随机/已压缩内容（图片/视频）。          |
| `deflate` | 优               | 中       | 较高           | 中等     | 文本响应巨大且带宽紧张，能接受 MCU 解压代价。   |
| `lz4`     | 中（约 0.5~0.7） | 低       | 极低（≈ memcpy）| 极小     | MCU 带宽敏感但要解压速度——LZ4 单循环就能解开。 |

---

## 7. 向后兼容性矩阵

| 插件 \ 快应用 | v1 (无 caps)         | v2 (caps.chunk=true，无 encodings) | v3 (full caps)                |
| ------------- | -------------------- | -------------------------------- | ----------------------------- |
| v1            | 单消息 `text`/`base64` | 单消息（v1 忽略 caps）              | 单消息（v1 忽略 caps）            |
| v2            | 单消息（保持 v1）       | 单消息 / 分片 base64               | 单消息 / 分片 base64（v2 不识别 encodings） |
| **v3 (本仓库)** | 单消息 `text`/`base64` | 单消息 base64 / 分片 base64        | 协商编码 + 协商压缩 + 自动分片   |

只要快应用对响应做到：
1. 先看 `resp.chunked` 决定是否等 chunk；
2. 先看 `resp.bodyEncoding` 决定如何把 `body`/`data` 解码成字节；
3. 再看 `resp.compression` 决定是否解压；
4. 最后看 `resp.raw` 决定是否 UTF-8 解码；

就能同时兼容 v1、v2、v3 三种插件。

---

## 8. 快应用接入示例

下面给出一份**框架无关**的参考实现。`transport` 是一层薄抽象，对接平台 API：

```js
// transport.js
// 平台相关：把 QAIC interconnect 包成 send(text) / onMessage(cb)。
// 不同 host 命名不同，常见有 @system.interconnect / interconn 等。
import interconn from '@system.interconnect';

export const transport = {
  send(text) {
    interconn.send({ data: text });
  },
  onMessage(handler) {
    interconn.subscribe((evt) => {
      const text = typeof evt === 'string' ? evt : (evt.data ?? evt.payloadText ?? '');
      try { handler(JSON.parse(text)); } catch (_) { /* 非 JSON，忽略 */ }
    });
  },
};
```

### 8.1 客户端核心实现

```js
// fetch-bridge-client.js
import { transport } from './transport.js';
// RTOS / 浏览器都常见的小型解压库；按你的运行时挑一个即可。
//   pako   — deflate / gzip
//   fflate — deflate / gzip / zip，更小
//   lz4js  — LZ4 block 解压
import { inflateRaw } from 'pako';      // 用于 deflate 解压
import { decompress as lz4Decompress } from 'lz4js'; // 用于 lz4 解压

// ---- 协议常量 ----
const HS_TAG          = '__hs__';
const FETCH_TAG       = 'fetch';
const FETCH_CHUNK_TAG = 'fetch-chunk';

// ---- 本端能力声明 ----
// 按偏好顺序排列：第一个是最希望对端使用的。
const LOCAL_CAPS = {
  version: 3,
  chunk: true,
  maxChunkSize: 4096,
  // RTOS 手表更喜欢 hex（极简解码）；台式/手机环境改成 ["base64","hex"] 更省带宽。
  encodings:   ['hex', 'base64'],
  // 想完全省 CPU 就只写 ['none']；要省带宽用 ['deflate','lz4','none']。
  compressions: ['lz4', 'none'],
};

// ---- 内部状态 ----
let nextReqId = 1;
const pending = new Map();      // id -> { resolve, reject, header, chunks, received }
let negotiated = { version: 1, chunked: false, chunkSize: 0, encodings: [], compressions: [] };

// ---- 解码器 ----
function decodeBody(text, encoding) {
  switch (encoding) {
    case 'text':   return new TextEncoder().encode(text); // text → bytes
    case 'base64': return b64decode(text);
    case 'hex':    return hexDecode(text);
    default:       throw new Error(`unknown bodyEncoding: ${encoding}`);
  }
}

function b64decode(s) {
  const bin = (typeof atob === 'function')
    ? atob(s)
    : Buffer.from(s, 'base64').toString('binary');
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i) & 0xff;
  return out;
}

function hexDecode(s) {
  const len = (s.length / 2) | 0;
  const out = new Uint8Array(len);
  for (let i = 0; i < len; i++) {
    out[i] = parseInt(s.substr(i * 2, 2), 16);
  }
  return out;
}

function decompressBytes(bytes, algo, originalBytes) {
  switch (algo || 'none') {
    case 'none':    return bytes;
    case 'deflate': return inflateRaw(bytes);
    case 'lz4':     return lz4Decompress(bytes, originalBytes); // lz4js 要传目标长度
    default:        throw new Error(`unknown compression: ${algo}`);
  }
}

function bytesToUtf8(bytes) {
  return new TextDecoder('utf-8').decode(bytes);
}

// ---- 握手 ----
function sendHandshake(count) {
  transport.send(JSON.stringify({ tag: HS_TAG, count, caps: LOCAL_CAPS }));
}

function intersectInOrder(peerList, localList) {
  if (!Array.isArray(peerList) || peerList.length === 0) return [];
  return peerList.filter((x) => localList.includes(x));
}

function negotiateCaps(peerCaps) {
  if (!peerCaps) {
    return { version: 1, chunked: false, chunkSize: 0, encodings: [], compressions: [] };
  }
  const version  = Math.min(peerCaps.version ?? 1, LOCAL_CAPS.version);
  const chunked  = !!peerCaps.chunk && LOCAL_CAPS.chunk && version >= 2;
  const peerMax  = peerCaps.maxChunkSize || LOCAL_CAPS.maxChunkSize;
  const chunkSize = chunked ? Math.max(256, Math.min(peerMax, LOCAL_CAPS.maxChunkSize)) : 0;
  // 对端建议的就是我们解码侧的能力反向参考；这里只保留对端声明并和本地求交。
  // 真正的发送方（插件）会再按对端偏好选——和这里的顺序无关。
  const encodings    = intersectInOrder(peerCaps.encodings,    LOCAL_CAPS.encodings);
  const compressions = intersectInOrder(peerCaps.compressions, LOCAL_CAPS.compressions);
  return { version, chunked, chunkSize, encodings, compressions };
}

function handleHandshake(msg) {
  negotiated = negotiateCaps(msg.caps);
  const count = (msg.count ?? 0) | 0;
  if (count < 2) sendHandshake(count + 1);
}

// ---- fetch 响应处理 ----
function inferEncoding(resp) {
  if (resp.bodyEncoding) return resp.bodyEncoding;
  // v1 兼容：raw 决定 base64 vs text
  return resp.raw ? 'base64' : 'text';
}

function finalizePending(id) {
  const slot = pending.get(id);
  if (!slot) return;

  // 1) 顺序拼装所有分片字节（解码后字节）
  const total = slot.header.totalBytes;
  const buf   = new Uint8Array(total);
  let offset  = 0;
  for (let i = 0; i < slot.header.chunkCount; i++) {
    const part = slot.chunks[i];
    if (!part) { slot.reject(new Error(`missing chunk ${i}`)); pending.delete(id); return; }
    buf.set(part, offset);
    offset += part.length;
  }
  if (offset !== total) {
    slot.reject(new Error(`length mismatch: got ${offset}, expected ${total}`));
    pending.delete(id);
    return;
  }

  // 2) 解压缩（如有）
  let bytes;
  try {
    bytes = decompressBytes(buf, slot.header.compression, slot.header.originalBytes);
  } catch (err) {
    slot.reject(err); pending.delete(id); return;
  }

  // 3) 二进制 vs 文本
  const body = slot.header.raw ? bytes : bytesToUtf8(bytes);
  slot.resolve(buildResp(slot.header, body));
  pending.delete(id);
}

function buildResp(header, body) {
  return {
    ok: header.ok,
    status: header.status,
    statusText: header.statusText,
    headers: header.headers || {},
    body,        // string 或 Uint8Array
    raw: !!header.raw,
  };
}

function handleFetchHeader(msg) {
  const slot = pending.get(msg.id);
  if (!slot) return;

  const resp = msg.resp || {};
  if (!resp.chunked) {
    // 单消息模式：直接解码 + 可能解压
    try {
      const enc  = inferEncoding(resp);
      const raw0 = decodeBody(resp.body ?? '', enc);
      const raw1 = decompressBytes(raw0, resp.compression, resp.originalBytes);
      const body = resp.raw ? raw1 : bytesToUtf8(raw1);
      slot.resolve(buildResp(resp, body));
    } catch (err) {
      slot.reject(err);
    }
    pending.delete(msg.id);
    return;
  }

  // 分片模式：暂存头部，等待 fetch-chunk
  slot.header  = resp;
  slot.chunks  = new Array(resp.chunkCount);
  slot.received = 0;
}

function handleFetchChunk(msg) {
  const slot = pending.get(msg.id);
  if (!slot || !slot.header) return;
  const seq = msg.seq | 0;
  if (slot.chunks[seq]) return; // 重复，忽略
  try {
    slot.chunks[seq] = decodeBody(msg.data || '', slot.header.bodyEncoding || 'base64');
  } catch (err) {
    slot.reject(err); pending.delete(msg.id); return;
  }
  slot.received += 1;
  if (slot.received >= slot.header.chunkCount) finalizePending(msg.id);
}

// ---- 入口：监听所有消息 ----
transport.onMessage((msg) => {
  switch (msg && msg.tag) {
    case HS_TAG:          handleHandshake(msg); break;
    case FETCH_TAG:       handleFetchHeader(msg); break;
    case FETCH_CHUNK_TAG: handleFetchChunk(msg); break;
    default:              /* 未知 tag：按协议要求忽略 */ break;
  }
});

// ---- 启动握手（可选；插件也会主动发起） ----
sendHandshake(0);

// ---- 对外 API ----
export function fetch(url, options = {}) {
  const id = String(nextReqId++);
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    transport.send(JSON.stringify({
      tag: FETCH_TAG,
      id,
      url,
      options,
    }));
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error('fetch timeout'));
      }
    }, 30_000);
  });
}
```

### 8.2 使用示例

```js
import { fetch } from './fetch-bridge-client.js';

// 文本 API：在 LOCAL_CAPS 含 'text' 时，插件会直接以文本回传，零解码开销
fetch('https://example.com/hello.json')
  .then((resp) => { if (resp.ok) console.log('text:', resp.body); });

// 二进制响应（大图/字体）：握手协商出 hex+lz4 时，会用 lz4 压缩 + hex 编码 + 分片
fetch('https://example.com/big.png', { raw: true })
  .then((resp) => { if (resp.ok) console.log('bytes:', resp.body.length); });
```

### 8.3 按设备能力调参

直接改 `LOCAL_CAPS` 即可，无需改动其它代码：

```js
// 极简 RTOS：宁愿多传字节也别让我解压/复杂解码
const LOCAL_CAPS = {
  version: 3,
  chunk: true,
  maxChunkSize: 2048,
  encodings:    ['hex'],           // 只能 hex
  compressions: ['none'],          // 不解压
};

// 带宽极差但 CPU 够：deflate 优先
const LOCAL_CAPS = {
  version: 3,
  chunk: true,
  maxChunkSize: 8192,
  encodings:    ['base64', 'hex'],
  compressions: ['deflate', 'lz4', 'none'],
};
```

### 8.4 最小集成（仅 v1，不分片不压缩）

如果不打算改造旧应用：

- 握手包**不带 `caps`** 或带 `{ chunk: false }`。
- 协商结果会是 `chunked=false, encodings=[], compressions=[]`，插件全程 v1 路径。
- 你只需处理 `tag === 'fetch'` 的单条响应，按 `raw` 决定是文本还是 base64。

---

## 9. 实现备注 (For Agents)

修改本协议时，参考以下源文件：

- `src/codec.rs:14-66` — `BodyEncoding` / `Compression` 枚举与 `parse` / `as_str`。
- `src/codec.rs:71-92` — `SUPPORTED_ENCODINGS` / `SUPPORTED_COMPRESSIONS` / `COMPRESS_MIN_SIZE` 常量。
- `src/codec.rs:94-117` — `compress(data, algo)` 与 `encode(data, encoding)`。
- `src/handshake.rs:19-30` — 常量 `LOCAL_PROTOCOL_VERSION / LOCAL_MAX_CHUNK_SIZE / MIN_CHUNK_SIZE`。
- `src/handshake.rs:65-87` — `PeerCaps` / `NegotiatedCaps` 数据结构。
- `src/handshake.rs:124-145` — `negotiated_caps(addr, pkg)` / `negotiated_chunk_size`，分片与编码的唯一查询点。
- `src/handshake.rs:148-200` — `handle_packet`、`parse_caps`、`negotiate` 协商主流程。
- `src/handshake.rs:265-281` — `local_caps_value`，对外声明本端能力。
- `src/fetch.rs:13-17` — `FETCH_TAG` / `FETCH_CHUNK_TAG` 常量。
- `src/fetch.rs:165-211` — `build_plan` / `pick_compression` / `pick_encoding`：响应编码策略入口。
- `src/fetch.rs:226-321` — `send_unchunked` / `send_chunked` 的 wire 字段写出顺序。

兼容性硬性约束：

1. **永远先发头部消息**：分片模式下 `tag:"fetch"` 头部必须先于任何 `fetch-chunk` 出门。
2. **不要重命名/重排** `resp.{ok,status,statusText,headers,body,raw}` 六个 v1 字段。
3. **未知字段必须可被旧端忽略**：所有 v2/v3 新增字段都是可选元信息；对没声明 `caps` 的对端，
   插件**禁止**输出 `chunked` / `bodyEncoding != legacy` / `compression != none`。
4. **`base64` 必须永远在 `SUPPORTED_ENCODINGS` 里**：它是兜底通用编码，任何对端都假定能解码。
5. **`none` 必须永远在 `SUPPORTED_COMPRESSIONS` 里**：是默认无压缩选项。
6. **`chunkSize` 必有下限**（当前 `MIN_CHUNK_SIZE=256`），避免恶意/异常 peer 让我们发出几万个微帧。
7. **`COMPRESS_MIN_SIZE` 阈值之下不压缩**：避免短包压缩反而膨胀。
8. 修改默认 `LOCAL_MAX_CHUNK_SIZE` 时要同时评估 QAIC 单帧上限 + 编码膨胀（hex 2×）+ JSON 外壳长度。
