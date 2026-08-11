# 网桥 FetchBridge 协议规范 (v4)

本协议描述 AstroBox NG 插件 **网桥 FetchBridge** 与运行在小米穿戴设备上的快应用之间，
通过 QAIC `interconnect` 通道交换 HTTP 请求/响应所使用的消息格式。

---

## 1. 协议版本

| 版本 | 状态     | 引入内容                                                                                  |
| ---- | -------- | ----------------------------------------------------------------------------------------- |
| v1   | 永久兼容 | 握手计数 ping-pong；fetch 单消息响应（文本直传 或 base64）。                              |
| v2   | 永久兼容 | 在 v1 基础上，握手包加入 `caps` 协商；新增 `fetch-chunk` 分片响应路径。                   |
| v3   | 永久兼容 | 在 v2 基础上：①握手 `caps` 加入 `encodings` / `compressions`；②加入 `ack` / `ackWindow` 和 `fetch-ack` 滑动窗口流控。 |
| v4   | 当前     | 在完整保留 v1-v3 的基础上，新增**开放长度、有限内存**的 HTTP 响应流；支持在线音频/视频、大文件、累计 ACK、逐帧 CRC32、重传、取消、结束和错误帧。 |

**兼容承诺**：

- v4 插件对未带 `caps` 或仅声明 v1/v2/v3 能力的快应用，**严格保持对应旧版本行为**；旧端永远不会收到任何 v4 tag 或字段。
- v3 的 `fetch-chunk` / `fetch-ack` 格式与状态机冻结并永久兼容。v4 使用独立 tag，不改变旧消息的语义。
- 只有双方满足 `version>=4 && stream=true && chunk=true && ack=true` 时才启用 v4 流。
- v4 插件对 v3 快应用仍按双方共同支持的能力交集 + 对端偏好顺序选择编码/压缩。
- 任何 v3 快应用都必须保留对 v1 单消息 `base64`/`text` 响应的处理能力——
  这是首响应到达前协商尚未完成时的兜底路径。
- 为保护 AstroBox UI 与 QAIC/BLE 传输，插件会拒绝超过 `MAX_UNCHUNKED_WIRE_LEN`
  的 legacy 单消息响应；大图/长响应必须先完成 `chunk=true` 协商后走分片。

### ⚠️ 关于分片死锁 (v2 → v3 的关键修复)

v2 的分片发送是**无流控**的：插件在**一次** `on_event` 调用里用一个紧凑 `for` 循环把所有
`fetch-chunk` 帧背靠背地（每帧一次阻塞式 `send_qaic_message`）灌进 QAIC/BLE 通道。响应一旦较大，
帧产出速度远超手表侧的消费速度；又因为插件在帧与帧之间**从不把控制权交还宿主**，宿主无法继续
泵送底层传输——发送队列被填满后永不排空，整笔传输**死锁**。

v3 用**累计 ACK + 滑动窗口**根治：发送方任一时刻最多让 `window` 个分片在途，发完即从 `on_event`
**返回**；手表每收到分片回送一个 `fetch-ack`（携带它下一片连续缺口的序号），ACK 推进窗口后才继续
发下一批。这样既给在途字节数封顶，又在每一批之间把控制权交还宿主让传输得以排空。详见 §5.2.1。

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
| `fetch-ack`   | 快应用 → 插件                                     | **v3**：有限分片的累计 ACK。 |
| `fetch-stream` | 插件 → 快应用                                    | **v4**：开放长度流的数据帧或最终帧。 |
| `fetch-stream-ack` | 快应用 → 插件                                | **v4**：流帧累计 ACK，用于背压与重传。 |
| `fetch-stream-cancel` | 快应用 → 插件                             | **v4**：主动取消并关闭 HTTP 数据源。 |
| `fetch-stream-error` | 插件 → 快应用                                | **v4**：HTTP 流读取过程中发生错误。 |

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
| `ack`          | boolean           | `false`    | 本端**是否会为分片响应回送 `fetch-ack`**。仅当快应用置 `true` 时，插件才启用滑动窗口流控；否则退回 v2 无流控分片。 |
| `ackWindow`    | integer           | 插件默认   | 本端希望的在途分片窗口（单位：分片数）。插件会与自身上限取 `min` 并夹到 `[1, 64]`。缺省/`0` 表示采用插件默认。 |
| `stream`       | boolean           | `false`    | **v4**：支持开放长度响应流、`fetch-stream-*` 控制帧、CRC32 和取消。必须同时声明 `chunk:true` 与 `ack:true`。 |

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
negotiated.ackWindow     = (version >= 3 && negotiated.chunked && local.ack && peer.ack)
                             ? clamp(peer.ackWindow || DEFAULT_ACK_WINDOW,
                                     MIN_ACK_WINDOW, MAX_ACK_WINDOW)
                             : 0
negotiated.stream        = version >= 4 && local.stream && peer.stream
                           && negotiated.chunked && negotiated.ackWindow > 0
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
LOCAL_PROTOCOL_VERSION   = 4
LOCAL_STREAM_SUPPORTED   = true
LOCAL_CHUNK_SUPPORTED    = true
LOCAL_MAX_CHUNK_SIZE     = 4096   bytes (压缩后)
MIN_CHUNK_SIZE           = 256    bytes
COMPRESS_MIN_SIZE        = 256    bytes  // 小于此阈值不压缩
LOCAL_ENCODINGS          = ["base64", "hex", "text"]
LOCAL_COMPRESSIONS       = ["none", "deflate", "lz4"]

LOCAL_ACK_SUPPORTED      = true
DEFAULT_ACK_WINDOW       = 4      chunks // 默认在途分片数
MIN_ACK_WINDOW           = 1      chunks
MAX_ACK_WINDOW           = 64     chunks

SESSION_IDLE_TIMEOUT     = 600    seconds // 握手/请求/ACK 任意活动都会刷新
MAX_UNCHUNKED_WIRE_LEN   = 16384  chars   // legacy 单消息响应体编码字符串上限
```

- 任意一端未声明 `caps` ⇒ 全部走 v1 路径（单消息、`text`/`base64`、不压缩、不分片、无 ACK）。
- 任意一端 `chunk=false` ⇒ 不分片。
- `version<2` ⇒ 不分片。
- `encodings`/`compressions` 缺失或交集为空 ⇒ 走 v1/v2 baseline 默认值。
- 快应用 `ack` 缺失或为 `false` ⇒ `ackWindow=0` ⇒ 分片走 v2 无流控路径（一次性发完所有分片）。
  **只有快应用显式声明 `ack:true`，本插件才会启用滑动窗口并等待 `fetch-ack`。**
- 已协商能力是会话状态，不是 3 秒内的一次性握手状态。插件在收到 `__hs__`、`fetch` 请求、
  `fetch-ack` 时都会刷新会话活跃时间；只有连续空闲超过 `SESSION_IDLE_TIMEOUT` 后才丢弃。
- 如果快应用在没有有效协商状态时直接请求大图，插件只能按 legacy 单消息兜底；当编码后的
  单消息体超过 `MAX_UNCHUNKED_WIRE_LEN` 时会返回错误，避免把整张图塞进一帧导致 UI/传输卡死。

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
    "raw":     false,
    "stream":  true
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
| `options.stream`    | boolean           | 否   | **v4**：`true` 强制流式；`false` 强制沿用有限 v1-v3 响应；缺省时对 `audio/*`、`video/*` 或 `Content-Length >= 64 KiB` 自动流式。未协商 v4 时忽略并安全回退旧路径。 |

> v3 当前**未对请求体引入压缩/分片**——上行请求通常很小、且来自更弱的设备。
> 若未来扩展，将在 `options` 里加 `bodyEncoding` / `bodyCompression` 字段，同样向后兼容。

> 大图请求前，快应用应先完成 `__hs__` 能力协商，并声明 `chunk:true`；若要避免大响应死锁，
> 还必须声明 `ack:true` 并按 §5.2.1 增量回 ACK。插件收到 fetch 时会主动保活/补发握手，
> 但同一次 `on_event` 内无法等待快应用的握手回包再重算响应计划。

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

**单消息保护线**：当插件没有可用分片协商，且编码后的 `body` 字符串超过
`MAX_UNCHUNKED_WIRE_LEN` 时，插件不会发送该大包，而是返回 §5.3 的错误响应。这样会让调用方
快速失败并重新握手/重试，避免单个超大 `fetch` 帧阻塞 AstroBox UI 或 QAIC 发送队列。

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
    "originalBytes": 65536,      // 可选；解压后字节数

    "ack":          true         // 可选；为 true 表示本次分片启用 ACK 流控，
                                 // 快应用必须按 §5.2.1 回送 fetch-ack。缺省/false ⇒ 无流控。
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
| `resp.ack`        | 可选 `true`，表示本次分片走 ACK 流控；快应用须回送 `fetch-ack`。缺省 ⇒ 无流控。 |
| chunk `seq`       | 分片序号，`0..chunkCount-1`。                                  |
| chunk `total`     | 冗余校验值，应与头部的 `chunkCount` 相等。                      |
| chunk `data`      | 本分片的 `bodyEncoding` 编码。解码后长度应为 `chunkSize`，最后一片可能更短。 |

**重组规则**：

1. 收到 `resp.chunked === true` 时，按 `id` 建立缓冲区，记录 `chunkCount/totalBytes/raw/bodyEncoding/compression/originalBytes`，并记下 `resp.ack` 是否为 `true`。
2. 每收到一个 `fetch-chunk`：按 `bodyEncoding` 解码 → 写入 `buffer[seq]`。**若 `resp.ack` 为 `true`，随即按 §5.2.1 回送一帧 `fetch-ack`。**
3. 全部 `chunkCount` 个分片到齐：拼接缓冲区，总长度应等于 `totalBytes`。
4. 若 `compression != "none"`：按算法解压缩，结果长度应等于 `originalBytes`（若提供）。
5. 若 `raw === false`：以 UTF-8 解码为字符串；否则保留为字节。

### 5.2.1 ACK 流控（滑动窗口）

> **动机**：见 §1「关于分片死锁」。无流控的背靠背发送会撑爆 QAIC/BLE 发送队列且不交还控制权，
> 导致大文件分片死锁。ACK 流控让发送方在途分片数封顶，并在每批之间返回宿主以排空传输。

**仅当**头部 `resp.ack === true`（即握手时双方都声明了 `ack` 能力）才启用。否则按 v2 无流控处理。

#### `fetch-ack` 帧（快应用 → 插件）

```json
{
  "tag": "fetch-ack",
  "id":  "<原 id>",
  "ack": 3
}
```

| 字段  | 说明                                                                                       |
| ----- | ------------------------------------------------------------------------------------------ |
| `id`  | 与所确认的分片响应同一个 `id`（无 `id` 时省略，按空串匹配）。                              |
| `ack` | **下一个仍缺失的连续分片序号** = 快应用已按序连续收到的分片数。即「`seq < ack` 的分片我全收到了」。当收齐全部时 `ack === chunkCount`。 |

**`ack` 的精确语义（累计确认）**：快应用维护一个「连续前沿」——从 `seq=0` 起最长的、无空洞的
已收区间长度 `k`，则 `ack = k`。例如已收到 `{0,1,3,4}`（缺 2）则 `ack=2`；待 2 补齐后，
`{0,1,2,3,4}` 连续 ⇒ `ack=5`。快应用**乱序缓存**分片（按 `seq` 落位），因此一旦空洞补上，
`ack` 会一次性跳到新的连续前沿。

#### 发送方（插件）窗口逻辑

设窗口大小 `W = negotiated.ackWindow`，维护 `base`（首个未确认分片 = 收到的最大 `ack`）与
`next`（下一个待发序号）：

```
开始：发送头部帧 → base=0, next=0 → 发送 seq ∈ [0, W) 的分片 → 返回(让出控制权)

收到 fetch-ack(ack)：
  ack ← min(ack, chunkCount)            // 防御越界
  若 ack > base:                        // 窗口前移
      base ← ack
      发送 seq ∈ [next, base+W) 且 < chunkCount 的分片，更新 next
  否则若 next > base（有在途未确认，且本 base 尚未重传过）:  // 对端停在 base，疑似丢片
      next ← base                       // go-back-N：回退重发整窗
      重发 seq ∈ [base, base+W)，并记下「已为此 base 重传」
  若 base ≥ chunkCount: 传输完成，释放该 id 的发送状态
```

- **稳态**：手表每收 1 片回 1 个 ACK，`base` 每次 +1，发送方补发 1 片，窗口始终填满、流水推进。
- **丢片恢复**：底层 QAIC/BLE 本身可靠，丢片罕见。万一发生，对端持续回送同一个 `ack`（停滞），
  发送方据此 go-back-N 回退重发该窗。**每个停滞点（`base` 值）只重传一次**——因为 `base` 单调递增，
  一次丢片产生的那一串重复 ACK 不会触发重传风暴；待 `base` 真正推进后，新的停滞点才允许再次重传。
  由于对端乱序缓存，补齐空洞后 `ack` 即跳过已缓存分片。
- **超时清理**：若某传输长时间（实现取 30s）无任何 ACK，发送方丢弃其发送状态以防内存泄漏；
  快应用侧亦应有 fetch 超时兜底。
- **会话保活**：每个 `fetch-ack` 同时刷新握手会话活跃时间，避免长图传输期间协商能力过期。
  这保证下一次图片请求仍能沿用 `chunk/ack` 能力，而不是退回 legacy 单消息路径。

#### 时序示意（`chunkCount=5, W=4`）

```
插件                                     快应用
 |-- fetch (header, ack:true) ----------->|  建缓冲, ack 模式
 |-- fetch-chunk seq=0 ------------------>|  收0 → 回 ack=1
 |-- fetch-chunk seq=1 ------------------>|  收1 → 回 ack=2
 |-- fetch-chunk seq=2 ------------------>|  收2 → 回 ack=3
 |-- fetch-chunk seq=3 ------------------>|  收3 → 回 ack=4
 |<-- fetch-ack ack=1 --------------------|
 |-- fetch-chunk seq=4 ------------------>|  (窗口随每个 ack 前移)
 |<-- fetch-ack ack=2 --------------------|
 |<-- fetch-ack ack=3 --------------------|
 |<-- fetch-ack ack=4 --------------------|  收4 → 回 ack=5 (=chunkCount)
 |<-- fetch-ack ack=5 --------------------|  插件：base≥chunkCount ⇒ 完成
```

> **对快应用的硬性要求**：必须**增量**回 ACK（每收到一个分片就回一次，或至少每 ⌈W/2⌉ 个回一次）。
> 若实现成「收齐全部才回一个 ACK」，当 `chunkCount > W` 时窗口会在第 `W` 个分片后停住，
> 再次死锁。增量 ACK 是 ACK 流控不死锁的前提。

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

## 6. v4 开放长度流

v4 流用于在线音频、在线视频和无法安全整体缓冲的大文件。它与 v3 有限分片并存：
`resp.chunked===true` 仍表示 v2/v3；`resp.stream===true` 才表示 v4。流式请求必须带非空且并发唯一的 `id`。

### 6.1 响应头

头部仍先使用 `tag:"fetch"`，并保留全部 v1 核心字段：

```json
{
  "tag": "fetch",
  "id": "media-1",
  "resp": {
    "ok": true,
    "status": 200,
    "statusText": "OK",
    "headers": { "content-type": "audio/mpeg" },
    "body": "",
    "raw": true,
    "stream": true,
    "chunkSize": 4096,
    "bodyEncoding": "base64",
    "compression": "none",
    "ack": true,
    "checksum": "crc32",
    "contentLength": 12345678
  }
}
```

`contentLength` 仅在源站给出有效 `Content-Length` 时存在；直播或 chunked HTTP 响应可以完全未知。
v4 当前固定逐帧 `compression:"none"`，避免跨帧压缩状态导致随机恢复和有限内存失效。源站自身的
HTTP 内容编码不会被插件解释，仍按 HTTP 响应字节透传。

### 6.2 数据与结束帧

```json
{
  "tag": "fetch-stream",
  "id": "media-1",
  "seq": 0,
  "data": "<base64 或 hex>",
  "crc32": "9a71bb4c"
}
```

- `seq` 从 0 严格递增；接收方可以暂存窗口内乱序帧，但只能按连续序列消费。
- `crc32` 是**编码前当前帧原始字节**的 IEEE CRC-32，小写 8 位十六进制。校验失败不得推进 ACK。
- 任一时刻插件只读取并缓存至多 `ackWindow * chunkSize` 的响应体数据（另加小量 JSON 编码临时内存）。

HTTP EOF 使用同一序列中的可靠最终帧表示；最终帧也必须 ACK：

```json
{
  "tag": "fetch-stream",
  "id": "media-1",
  "seq": 417,
  "data": "",
  "crc32": "00000000",
  "final": true,
  "totalBytes": 1703936
}
```

`totalBytes` 是所有非最终帧解码后的累计字节数。接收方只有在最终帧之前的所有序号连续到齐、
CRC 全部通过后，才可提交 EOF。

### 6.3 ACK、背压与重传

```json
{ "tag": "fetch-stream-ack", "id": "media-1", "ack": 12 }
```

`ack` 与 v3 一样表示“下一个缺失的连续帧序号”，即所有 `seq < ack`（包括可能的最终帧）均已接收。
发送方只在 ACK 前移后释放对应缓冲并从 HTTP source 再读取恰好足以补满窗口的数据，因此慢速播放端
会自然向 HTTP 下载端施加背压。重复 ACK 会让发送方对当前未确认窗口执行至多一次 go-back-N 重传；
ACK 前沿推进后才允许下一次重传。v4 不允许无 ACK 流式发送。

### 6.4 取消、错误与清理

接收方停止播放、seek 到新 URL 或销毁页面时应发送：

```json
{ "tag": "fetch-stream-cancel", "id": "media-1", "reason": "player closed" }
```

插件立即删除状态并 drop HTTP response，从而关闭数据源。HTTP body 在头部之后读取失败时，插件发送：

```json
{ "tag": "fetch-stream-error", "id": "media-1", "message": "read streaming body failed: ..." }
```

流连续 30 秒未收到 ACK 会被清理。插件每 5 秒通过宿主 Timer 主动执行清理，因此即使此后没有任何
协议消息，遗留 source 也不会永久占用。迟到的 ACK/cancel 必须静默忽略。

### 6.5 有限内存和兼容硬门控

1. 只有 `version>=4 && stream && chunk && ack` 的共同协商结果才能产生 v4 帧。
2. v1/v2/v3 peer 不会收到 `resp.stream` 或任何 `fetch-stream-*` tag。
3. `options.stream:true` 在旧会话中不报协议错误，而是回退到原 v1-v3 有限响应规则。
4. v4 流不能缺少 `id`；插件返回普通 v1 形态错误，而不会以空 id 建立共享状态。
5. 每帧先校验 CRC，再推进累计 ACK；最终帧同样占一个序号并受 ACK/重传保护。
6. 头部永远先于首个数据帧发送。

---

## 7. 编码与压缩方式参考

### 7.1 编码 `bodyEncoding`

| 编码     | 膨胀率 | 解码复杂度       | 适合场景                                        |
| -------- | ------ | ---------------- | ----------------------------------------------- |
| `text`   | 1.0×   | 零                | 文本 API（JSON、HTML）且未压缩、未分片。        |
| `base64` | ~1.33× | 查表 + 位移      | 通用二进制（兼容性最好，v1/v2 基线）。           |
| `hex`    | 2.0×   | 两次查表（极简） | RTOS / 低算力 MCU，宁愿多传一倍字节也要降解码开销。 |

### 7.2 压缩 `compression`

| 算法      | 压缩率           | 编码 CPU | 解码 CPU       | 内存占用 | 适合场景                                       |
| --------- | ---------------- | -------- | -------------- | -------- | ---------------------------------------------- |
| `none`    | 1.0              | 零       | 零             | 零       | 短响应、随机/已压缩内容（图片/视频）。          |
| `deflate` | 优               | 中       | 较高           | 中等     | 文本响应巨大且带宽紧张，能接受 MCU 解压代价。   |
| `lz4`     | 中（约 0.5~0.7） | 低       | 极低（≈ memcpy）| 极小     | MCU 带宽敏感但要解压速度——LZ4 单循环就能解开。 |

---

## 8. 向后兼容性矩阵

| 插件 \\ 快应用 | v1（无 caps） | v2 | v3（ack） | v4（stream+ack） |
| --- | --- | --- | --- | --- |
| v1 | v1 单消息 | v1 单消息 | v1 单消息 | v1 单消息 |
| v2 | v1 单消息 | v2 有限分片 | v2 有限分片 | v2 有限分片 |
| v3 | v1 单消息 | v2 有限分片 | v3 编码/压缩 + ACK | v3（忽略 stream） |
| **v4（本仓库）** | **严格 v1** | **严格 v2** | **严格 v3** | 显式协商后使用 v4 有限内存开放流；否则仍为 v3 |

> 注意「v3 (无 ack)」一列：快应用即便声明了 v3 的 `encodings`/`compressions`，只要没置 `ack:true`，
> 分片仍走 v2 式无流控发送——大文件仍有死锁风险。要彻底消除死锁，**必须**声明 `ack:true`（见 §5.2.1）。

只要快应用对响应做到：
1. 先看 `resp.chunked` 决定是否等 chunk；**若 `resp.ack` 为 `true`，每收到一片就回送 `fetch-ack`（§5.2.1）**；
2. 先看 `resp.bodyEncoding` 决定如何把 `body`/`data` 解码成字节；
3. 再看 `resp.compression` 决定是否解压；
4. 最后看 `resp.raw` 决定是否 UTF-8 解码；

就能同时兼容 v1、v2、v3；v4 客户端再按 §6 处理 `resp.stream` 和 `fetch-stream-*`。

---

## 9. 快应用接入示例

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

### 9.1 客户端核心实现

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
const FETCH_ACK_TAG   = 'fetch-ack';
const FETCH_STREAM_TAG = 'fetch-stream';
const FETCH_STREAM_ACK_TAG = 'fetch-stream-ack';
const FETCH_STREAM_CANCEL_TAG = 'fetch-stream-cancel';
const FETCH_STREAM_ERROR_TAG = 'fetch-stream-error';

// ---- 本端能力声明 ----
// 按偏好顺序排列：第一个是最希望对端使用的。
const LOCAL_CAPS = {
  version: 4,
  stream: true,
  chunk: true,
  maxChunkSize: 4096,
  // RTOS 手表更喜欢 hex（极简解码）；台式/手机环境改成 ["base64","hex"] 更省带宽。
  encodings:   ['hex', 'base64'],
  // 想完全省 CPU 就只写 ['none']；要省带宽用 ['deflate','lz4','none']。
  compressions: ['lz4', 'none'],
  // 声明会回送 fetch-ack（启用滑动窗口流控，避免大文件分片把通道灌死）。
  // ackWindow 是希望的在途分片数；插件会与自身上限取 min。省略则用插件默认。
  ack: true,
  ackWindow: 4,
};

// ---- 内部状态 ----
let nextReqId = 1;
// id -> { resolve, reject, header, chunks, received, ackPaced, ackedUpto }
const pending = new Map();
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
  if (resp.stream) {
    slot.header = resp;
    slot.streamFrames = new Map();
    slot.streamNext = 0;
    slot.streamEnded = false;
    return;
  }
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
  slot.header   = resp;
  slot.chunks   = new Array(resp.chunkCount);
  slot.received = 0;
  // resp.ack === true 时插件启用了滑动窗口流控，我们必须增量回送 fetch-ack。
  slot.ackPaced  = resp.ack === true;
  slot.ackedUpto = 0;          // 当前连续前沿（= 下一个仍缺失的 seq）
}

// 回送累计 ACK：ack = 已按序连续收到的分片数（= 下一个仍缺失的 seq）。
function sendAck(id, ack) {
  const msg = { tag: FETCH_ACK_TAG, ack };
  if (id !== undefined) msg.id = id;
  transport.send(JSON.stringify(msg));
}

function handleFetchChunk(msg) {
  const slot = pending.get(msg.id);
  if (!slot || !slot.header) return;
  const seq = msg.seq | 0;
  if (slot.chunks[seq]) {
    // 重复分片（多半是 go-back-N 重传）。数据忽略，但仍要回当前 ACK，
    // 好让发送方知道我们的连续前沿，避免它继续停滞。
    if (slot.ackPaced) sendAck(msg.id, slot.ackedUpto);
    return;
  }
  try {
    slot.chunks[seq] = decodeBody(msg.data || '', slot.header.bodyEncoding || 'base64');
  } catch (err) {
    slot.reject(err); pending.delete(msg.id); return;
  }
  slot.received += 1;

  if (slot.ackPaced) {
    // 推进连续前沿：从当前 ackedUpto 起，把所有已落位的分片走完。
    while (slot.chunks[slot.ackedUpto]) slot.ackedUpto += 1;
    sendAck(msg.id, slot.ackedUpto);
  }

  if (slot.received >= slot.header.chunkCount) finalizePending(msg.id);
}

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let i = 0; i < 8; i++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  }
  return ((crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, '0');
}

function sendStreamAck(id, ack) {
  transport.send(JSON.stringify({ tag: FETCH_STREAM_ACK_TAG, id, ack }));
}

function handleFetchStream(msg) {
  const slot = pending.get(msg.id);
  if (!slot || !slot.header || !slot.header.stream) return;
  let bytes;
  try {
    bytes = decodeBody(msg.data || '', slot.header.bodyEncoding || 'base64');
    if (crc32(bytes) !== msg.crc32) throw new Error(`CRC mismatch at stream frame ${msg.seq}`);
  } catch (err) {
    slot.reject(err);
    transport.send(JSON.stringify({ tag: FETCH_STREAM_CANCEL_TAG, id: msg.id, reason: String(err) }));
    pending.delete(msg.id);
    return;
  }
  if (!slot.streamFrames.has(msg.seq)) slot.streamFrames.set(msg.seq, { bytes, final: msg.final === true });
  while (slot.streamFrames.has(slot.streamNext)) {
    const frame = slot.streamFrames.get(slot.streamNext);
    slot.streamFrames.delete(slot.streamNext++);
    if (frame.final) {
      slot.streamEnded = true;
      slot.resolve(buildResp(slot.header, undefined));
      pending.delete(msg.id);
      break;
    }
    if (slot.onChunk) slot.onChunk(frame.bytes);
  }
  sendStreamAck(msg.id, slot.streamNext);
}

function handleFetchStreamError(msg) {
  const slot = pending.get(msg.id);
  if (!slot) return;
  slot.reject(new Error(msg.message || 'stream failed'));
  pending.delete(msg.id);
}

// ---- 入口：监听所有消息 ----
transport.onMessage((msg) => {
  switch (msg && msg.tag) {
    case HS_TAG:          handleHandshake(msg); break;
    case FETCH_TAG:       handleFetchHeader(msg); break;
    case FETCH_CHUNK_TAG: handleFetchChunk(msg); break;
    case FETCH_STREAM_TAG: handleFetchStream(msg); break;
    case FETCH_STREAM_ERROR_TAG: handleFetchStreamError(msg); break;
    default:              /* 未知 tag：按协议要求忽略 */ break;
  }
});

// ---- 启动握手（可选；插件也会主动发起） ----
sendHandshake(0);

// ---- 对外 API ----
export function fetch(url, options = {}) {
  const id = String(nextReqId++);
  const { onChunk, ...wireOptions } = options;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject, onChunk });
    transport.send(JSON.stringify({
      tag: FETCH_TAG,
      id,
      url,
      options: wireOptions,
    }));
    setTimeout(() => {
      if (pending.has(id)) {
        const slot = pending.get(id);
        if (slot && slot.header && slot.header.stream) {
          transport.send(JSON.stringify({ tag: FETCH_STREAM_CANCEL_TAG, id, reason: 'timeout' }));
        }
        pending.delete(id);
        reject(new Error('fetch timeout'));
      }
    }, 30_000);
  });
}
```

### 9.2 使用示例

```js
import { fetch } from './fetch-bridge-client.js';

// 文本 API：在 LOCAL_CAPS 含 'text' 时，插件会直接以文本回传，零解码开销
fetch('https://example.com/hello.json')
  .then((resp) => { if (resp.ok) console.log('text:', resp.body); });

// 二进制响应（大图/字体）：握手协商出 hex+lz4 时，会用 lz4 压缩 + hex 编码 + 分片
fetch('https://example.com/big.png', { raw: true })
  .then((resp) => { if (resp.ok) console.log('bytes:', resp.body.length); });

// v4 在线媒体/大文件：每个通过 CRC 校验且连续的块立即交给播放器，
// 不在手表上组装完整文件。Promise 在可靠 final 帧到达后完成。
fetch('https://example.com/live/audio.mp3', {
  raw: true,
  stream: true,
  onChunk(bytes) { player.append(bytes); },
}).then(() => player.end());
```

### 9.3 按设备能力调参

直接改 `LOCAL_CAPS` 即可，无需改动其它代码：

```js
// 极简 RTOS：宁愿多传字节也别让我解压/复杂解码
const LOCAL_CAPS = {
  version: 4,
  stream: true,
  chunk: true,
  maxChunkSize: 2048,
  encodings:    ['hex'],           // 只能 hex
  compressions: ['none'],          // 不解压
  ack: true,                       // 仍要开 ACK 流控，否则大文件分片可能灌死通道
  ackWindow: 2,                    // RAM 紧张 ⇒ 在途分片更少
};

// 带宽极差但 CPU 够：deflate 优先
const LOCAL_CAPS = {
  version: 4,
  stream: true,
  chunk: true,
  maxChunkSize: 8192,
  encodings:    ['base64', 'hex'],
  compressions: ['deflate', 'lz4', 'none'],
  ack: true,
  ackWindow: 8,                    // RAM/带宽充裕 ⇒ 更大窗口、更高吞吐
};
```

> **强烈建议任何会接收大响应的快应用都置 `ack: true`**：这是分片不死锁的根本保证。
> 省略 `ack` 只会退回 v2 无流控分片——小响应能用，大响应有撑爆通道的风险。
> 启用后唯一的额外成本就是 §8.1 里那几行 `sendAck`（收到分片即回一个累计序号）。

### 9.4 最小集成（仅 v1，不分片不压缩）

如果不打算改造旧应用：

- 握手包**不带 `caps`** 或带 `{ chunk: false }`。
- 协商结果会是 `chunked=false, encodings=[], compressions=[]`，插件全程 v1 路径。
- 你只需处理 `tag === 'fetch'` 的单条响应，按 `raw` 决定是文本还是 base64。

---

## 10. 实现备注 (For Agents)

修改本协议时，参考以下源文件：

- `src/codec.rs:13-72` — `BodyEncoding` / `Compression` 枚举与 `parse` / `as_str`。
- `src/codec.rs:76-92` — `SUPPORTED_ENCODINGS` / `SUPPORTED_COMPRESSIONS` / `COMPRESS_MIN_SIZE` 常量。
- `src/codec.rs:94-125` — `compress(data, algo)` 与 `encode(data, encoding)`。
- `src/handshake.rs:16-49` — 常量 `SESSION_IDLE_TIMEOUT / LOCAL_PROTOCOL_VERSION / LOCAL_MAX_CHUNK_SIZE / MIN_CHUNK_SIZE` 与 ACK 流控的 `LOCAL_ACK_SUPPORTED / DEFAULT_ACK_WINDOW / MIN_ACK_WINDOW / MAX_ACK_WINDOW`。
- `src/handshake.rs:80-106` — `PeerCaps` / `NegotiatedCaps` 数据结构（含 `ack` / `ack_window`）。
- `src/handshake.rs:153-200` — `is_open` / `record_activity` / `negotiated_caps(addr, pkg)`，会话保活与分片/编码/`ack_window` 的唯一查询点。
- `src/handshake.rs:202-278` — `handle_packet`、`ensure_open`、`parse_caps`、`negotiate` 协商主流程。
- `src/handshake.rs:368-380` — `local_caps_value`，对外声明本端能力（含 `ack` / `ackWindow`）。
- `src/fetch.rs:14-28` — `FETCH_TAG` / `FETCH_CHUNK_TAG` / `FETCH_ACK_TAG` / `MAX_UNCHUNKED_WIRE_LEN` 常量。
- `src/fetch.rs:186-216` — `build_plan` / `pick_compression` / `pick_encoding`：响应编码与 `ack_window` 决策入口。
- `src/fetch.rs:293-477` — `send_unchunked` / `send_chunked`（含 `ack:true` 头部标志与 ACK/无流控分支）/ `handle_ack`。
- `src/transfer.rs` — **ACK 流控状态机**：在途分片注册表、滑动窗口 `pump`、`begin`（首批发送）、`on_ack`（推进窗口/go-back-N 重传/完成清理）。死锁修复的核心。
- `src/stream.rs` — **v4 有限内存流状态机**：持有 HTTP source、窗口内未确认帧、CRC32、最终帧、重传、取消与超时清理。
- `src/lib.rs:dispatch_interconnect` — v3/v4 ACK 与取消 tag 的异步分发入口。

兼容性硬性约束：

1. **永远先发头部消息**：分片模式下 `tag:"fetch"` 头部（含 `ack` 标志）必须先于任何 `fetch-chunk` 出门。
2. **不要重命名/重排** `resp.{ok,status,statusText,headers,body,raw}` 六个 v1 字段。
3. **未知字段必须可被旧端忽略**：所有 v2/v3/v4 新增字段都是可选元信息；对没声明对应 `caps` 的对端，
   插件**禁止**输出 `chunked` / `bodyEncoding != legacy` / `compression != none` / `ack`。
4. **`base64` 必须永远在 `SUPPORTED_ENCODINGS` 里**：它是兜底通用编码，任何对端都假定能解码。
5. **`none` 必须永远在 `SUPPORTED_COMPRESSIONS` 里**：是默认无压缩选项。
6. **`chunkSize` 必有下限**（当前 `MIN_CHUNK_SIZE=256`），避免恶意/异常 peer 让我们发出几万个微帧。
7. **`COMPRESS_MIN_SIZE` 阈值之下不压缩**：避免短包压缩反而膨胀。
8. 修改默认 `LOCAL_MAX_CHUNK_SIZE` 时要同时评估 QAIC 单帧上限 + 编码膨胀（hex 2×）+ JSON 外壳长度。
9. **ACK 流控只在 `negotiated.ackWindow > 0` 时启用**（即双方都声明 `ack`）；否则分片必须走无流控老路，
   绝不能等待一个永远不会来的 `fetch-ack`。`ackWindow` 必有上下限（`[1, 64]`）：下限防瞬间停滞，上限防退化回无界 blast。
10. **发送帧绝不持有发送状态锁**：状态机在锁内只挑选帧/推进游标，释放锁后再 `.await` 宿主
    `send_qaic_message` future，避免跨挂起点持锁或发生重入死锁。
11. **不要在一次 `on_event` 里同步发完所有分片**：ACK 流控每批最多发 `window` 个就返回，靠后续 `fetch-ack`
    续传，从而把控制权交还宿主让传输排空。回退到无界 blast 会重新引入死锁。
12. **不要把超大响应退回 legacy 单消息**：缺失协商时，`send_unchunked` 必须用 `MAX_UNCHUNKED_WIRE_LEN`
    拦截大包并返回错误；否则一次错误的能力过期就会重新把整张图片塞进 `tag:"fetch"`。
13. **长传输必须刷新会话活跃时间**：`fetch` 请求和 v3/v4 ACK 都要调用握手保活逻辑，避免传输
    还在正常推进时协商能力被误判过期。
14. **`wit_bindgen::spawn` 不是线程池**：每个导出事件只创建一个组件任务，并由其返回的 `FutureReader`
    维持生命周期；协议处理和宿主 future 在同一任务内按序 `.await`。禁止为每个帧单独 `spawn`，否则会破坏头部/序号顺序。
15. **v4 生产必须由事件被动推进**：首窗之后只允许由 `fetch-stream-ack` 读取下一窗；不得创建永久循环
    把 `wit_bindgen::spawn` 当作 `tokio::spawn` 使用。Timer 只做有界清理，不负责持续抽取数据。
16. 当前 `waki 0.5.1` 的 HTTP `send/chunk` 本身使用 WASI blocking pollable；本实现已避免全量读取和无界循环，
    但若源站长期无新字节，单次 ACK 事件的 HTTP chunk 读取仍可能占用组件执行。要达到严格“可挂起而不阻塞”的直播源，
    AstroBox Host 需要提供返回 WIT `future` 的 HTTP stream-read/import，再在同一 `FutureReader` 任务内 `.await`；不得用额外 `spawn` 伪装成异步。
