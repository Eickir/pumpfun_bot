# 🧠 Solana Pump.fun Trading Bot

An automated **high-frequency trading bot** designed for **Solana**, specifically targeting **Pump.fun tokens**.  
The bot connects to a real-time **gRPC feed** (such as [Helius](https://www.helius.xyz/), [SVS](https://www.solanavibestation.com/), or [Shyft](https://shyft.to/dashboard/pricing)) to subscribe to transaction and block events.  
It performs automatic token trades, routes events through dedicated workers, and handles real-time strategy execution.

---

## 🚨 Requirements

> **Important: This bot requires a paid gRPC feed** to operate.

To run this bot, you must have access to a **Yellowstone-compatible gRPC streaming provider**, such as:

- [Helius](https://www.helius.xyz/)
- [SVS](https://www.solanavibestation.com/)
- [Shyft](https://shyft.to/dashboard/pricing)

These services provide real-time access to:

- All Solana transaction data
- Pump.fun trading events
- Your own wallet activity

You must configure your **gRPC endpoint and API key** via environment variables (see below).

---

## 🧪 Key Features

- ⚡ Real-time monitoring of Pump.fun token events
- 🧠 Decodes custom events from Solana transaction logs (e.g. trades, mints)
- 🔄 Auto-trading via per-token workers
- 🧵 Multithreaded architecture using Tokio
- 📡 Full gRPC integration with reconnection and filtering

---

## 🧬 Architecture Overview

```bash
src/
├── main.rs                         # Bot entrypoint
├── modules/
│   ├── grpc_configuration/         # gRPC client setup and streaming logic
│   │   ├── client.rs               # Subscribes to Pump.fun + wallet streams
│   │   └── constants.rs            # Network settings and Pump.fun program ID
│   ├── token_manager/              # Token trade logic and worker management
│   │   └── token_manager.rs
│   ├── utils/                      # Borsh decoding and data modeling
│   │   ├── decoder.rs              # Efficient decoding of Solana logs
│   │   └── types.rs                # Trade/Create event structs and records
│   ├── wallet/                     # (optional module for wallet-specific logic)
│   └── monitoring/                 # (optional monitoring tools)
```