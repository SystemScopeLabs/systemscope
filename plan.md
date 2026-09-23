1\. 프로젝트 정의

하드웨어부터 OS, 런타임, 네트워크, GPU까지 컴퓨터 시스템 전체를 모듈 단위로 연결하고, 실행 상태와 이벤트를 실시간 추적·시각화하는 인터랙티브 시스템 플랫폼.

핵심은 설명이 아니라:

실행 / 상태 / 이벤트 / 추적 / 교체 가능한 구현체

2\. 최종 형태

System

├─ Platform

│  ├─ Motherboard

│  ├─ PCIe

│  ├─ Firmware

│  └─ Power / Clock

│

├─ Compute

│  ├─ CPU

│  ├─ GPU

│  └─ Accelerator

│

├─ Memory

│  ├─ Cache

│  ├─ RAM

│  ├─ VRAM

│  └─ Virtual Memory

│

├─ Storage

│  ├─ NVMe SSD

│  ├─ HDD

│  ├─ Partition

│  └─ Filesystem

│

├─ I/O

│  ├─ USB

│  ├─ Keyboard

│  ├─ Display

│  └─ Interrupt / DMA

│

├─ Network

│  ├─ NIC

│  ├─ Ethernet

│  ├─ IP

│  ├─ TCP/UDP

│  └─ Socket

│

└─ Software

&#x20;  ├─ Boot

&#x20;  ├─ Kernel

&#x20;  ├─ Process / Thread

&#x20;  ├─ Scheduler

&#x20;  ├─ Driver

&#x20;  ├─ Runtime

&#x20;  └─ Application

사용자는 어느 계층이든 클릭해서 내부로 내려감.

3\. 핵심 기능

System Explorer

컴퓨터 전체 topology를 탐색.

Computer → CPU → Core → Pipeline → ALU

처럼 계속 내려감.

Execution Trace

프로그램 하나를 실행하면 시스템 전체에서 발생하는 사건을 추적.

PROCESS\_CREATE

PAGE\_FAULT

NVME\_READ

PAGE\_LOAD

CPU\_FETCH

TLB\_MISS

L1\_MISS

DRAM\_READ

SYSCALL\_ENTER

INTERRUPT

DMA\_COMPLETE

PACKET\_TX

GPU\_DISPATCH

State Inspector

특정 시점의 실제 상태를 봄.

CPU:

PC

Registers

Pipeline

ROB

Cache

TLB

Process:

PID

Thread

Virtual Address Space

Page Table

FD

State

SSD:

Queue

LBA

Namespace

Controller

NAND mapping

Timeline

Play

Pause

Step

Step Back

Seek

Breakpoint

Filter

결국 시스템 전체용 debugger처럼 동작하는 거야.

4\. 프로젝트의 중심

본체는 UI도 CPU도 아님.

Simulation Runtime

모든 컴포넌트를 하나의 실행 모델로 묶음.

&#x20;                   Runtime

&#x20;                      │

&#x20;       ┌──────────────┼─────────────┐

&#x20;       ▼              ▼             ▼

&#x20;      CPU            Memory        Storage

&#x20;       │              │             │

&#x20;       ├────── Events / State ──────┤

&#x20;       │              │             │

&#x20;       ▼              ▼             ▼

&#x20;       OS            GPU          Network

Runtime이 담당:

global time

event ordering

component lifecycle

concurrency

scheduling

state snapshots

deterministic replay

breakpoint

trace recording

여기가 사실 가장 중요한 엔진.

5\. Contracts

별도 레포 유지.

systemscope-contracts

여기서 정의:

Component

State

Event

Command

Trace

Capability

Topology

Time

Protocol

예:

MemoryReadRequested

MemoryReadCompleted

InstructionFetched

InstructionRetired

InterruptRaised

DmaStarted

DmaCompleted

PacketReceived

KernelEntered

GpuKernelDispatched

중요한 건 특정 CPU나 특정 OS에 종속되지 않는 것.

6\. 구현체와 플랫폼 분리

예를 들어 CPU라는 개념과 CPU 구현은 분리.

CPU Contract

&#x20;   │

&#x20;   ├─ Simple CPU Model

&#x20;   ├─ Rust CPU Simulator

&#x20;   ├─ SystemVerilog CPU

&#x20;   ├─ QEMU Adapter

&#x20;   └─ Real Trace Adapter

Memory도:

Memory Contract

&#x20;   │

&#x20;   ├─ Simple RAM

&#x20;   ├─ DDR Model

&#x20;   ├─ NUMA Model

&#x20;   └─ Trace-backed Model

이 구조여야 나중에 실제 구현을 계속 붙일 수 있음.

7\. Fidelity

여기서 중요한 수정.

교육용 level이 아니라 simulation fidelity level로 둬.

F0 Structural

구조와 연결만 표현



F1 Functional

입출력 동작 정확



F2 Architectural

ISA / memory / OS semantics 반영



F3 Microarchitectural

pipeline / cache / ROB / scheduler 반영



F4 RTL

SystemVerilog 수준



F5 Logic

gate / register / mux 수준



F6 Physical

standard cell / transistor / timing 수준

그리고 모든 컴포넌트가 같은 fidelity일 필요도 없음.

예:

CPU      F3

RAM      F2

SSD      F1

OS       F2

GPU      F1

Network  F2

이런 식으로 조합 가능.

이게 훨씬 강력해.

8\. 실제 데이터도 받을 수 있게

나중에는 simulation만 하지 않고 real trace backend를 붙임.

QEMU

Linux perf

eBPF

ETW

SystemVerilog simulator

PCIe trace

GPU profiler

Network capture

↓

공통 contract로 변환

↓

SystemScope Timeline

그러면 같은 UI에서

시뮬레이션 결과와 실제 시스템 trace를 둘 다 볼 수 있음.

이게 프로젝트를 단순 simulator보다 훨씬 크게 만들어.

9\. 언어

Rust

→ simulation runtime / core engines



TypeScript

→ visualizer / desktop-web UI



SystemVerilog

→ RTL hardware implementations



C / C++

→ kernel / driver / runtime / native adapter



Assembly

→ boot / interrupt / context switch / ISA-level code



Python

→ reference model / analysis / verification / tooling



Schema / Protobuf

→ language-neutral contracts

언어를 여러 개 쓰는 이유도 명확함.

각 시스템 계층의 실제 성격에 맞는 언어를 사용한다.

10\. 멀티레포

systemscope-contracts

systemscope-runtime

systemscope-visualizer



systemscope-cpu

systemscope-memory

systemscope-storage

systemscope-os

systemscope-network

systemscope-gpu

systemscope-platform



systemscope-hardware

systemscope-trace

systemscope-integration

나중에:

systemscope-windows

systemscope-linux

systemscope-cuda

systemscope-rocm

systemscope-riscv

systemscope-x86

같이 확장.

11\. 첫 구현

최종 목표를 축소하는 게 아니라 첫 번째 완전한 시스템 slice만 자름.

CPU

RAM

NVMe SSD

Process

Minimal OS model

프로그램 하나 실행:

Executable

→ Storage

→ RAM

→ Process

→ CPU

→ Memory

→ Syscall

→ Kernel

→ Output

단, 이걸 특정 시나리오 하드코딩으로 만들지 않고 모든 단계가 실제 component + event + state 모델로 동작하게 함.

그다음 같은 runtime에 GPU나 NIC를 꽂는 것.

12\. 최종 정체성

이제 이름도 이렇게 봐야 함.

Interactive Computer Systems Simulation \& Observability Platform

또는 더 강하게:

Full-Stack Computer Systems Digital Twin Framework

다만 Digital Twin은 실제 장비와 지속적으로 동기화하는 단계까지 가야 정확하니까 초기에는 Systems Simulation Platform이 제일 적절해.

핵심은 이거야.

컴퓨터를 설명하는 프로그램이 아니라, 컴퓨터 시스템 자체를 모듈화된 실행 모델로 구성하고 관측하는 플랫폼.

이 방향이면 CUDA/ROCm, Windows boot, 네트워크, 실제 trace, SystemVerilog까지 전부 한 기획 안에 자연스럽게 들어온다.



지금 기획이면 SystemScope가 제일 잘 맞아. 이미 의미도 정확해.

시스템 전체를 scope해서 구조·상태·이벤트를 본다.

멀티레포는 이렇게 가면 깔끔함.

systemscope-contracts

systemscope-runtime

systemscope-visualizer

systemscope-platform

systemscope-cpu

systemscope-memory

systemscope-storage

systemscope-os

systemscope-network

systemscope-gpu

systemscope-trace

systemscope-integration

나중에 확장:

systemscope-linux

systemscope-windows

systemscope-riscv

systemscope-cuda

systemscope-rocm

GitHub Organization까지 만든다면:

SystemScope/

├─ contracts

├─ runtime

├─ visualizer

├─ cpu

├─ memory

...

처럼 조직명이 이미 SystemScope면 각 레포에 systemscope- prefix를 굳이 반복하지 않는 방식도 더 예쁨.

내 선택은 프로젝트명 SystemScope + GitHub org SystemScope + repo는 contracts, runtime, cpu... 이 조합.

