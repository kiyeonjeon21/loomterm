## 2026-07 구현 검증: substrate의 최소 신뢰 경계

v0.2에서는 최초 prototype의 핵심 약점을 먼저 닫았다.

- command마다 private supervisor가 process group을 소유한다. daemon이
  `SIGKILL`되어 control pipe가 닫히면 supervisor가 TERM/KILL 순서로 명령을
  종료하고, 재시작한 daemon은 record를 `interrupted`로 확정한다.
- graceful shutdown은 새 실행을 거부하고 queued record를 취소한 뒤 running
  supervisor와 SQLite flush barrier를 기다린다.
- protocol v2의 cursor subscription은 durable replay 뒤 live event를 push한다.
  연결을 끊고 같은 cursor로 다시 붙는 통합 테스트에서 sequence 누락과 중복이
  없음을 확인했다.
- SQLite는 Tokio worker가 아니라 전용 storage actor thread에서 동작한다.
- 실제 Codex가 project-scoped MCP를 통해 stdout/stderr와 exit code 7을 읽고,
  장기 실행을 cancel한 뒤 terminal state까지 재조회하는 smoke test를 통과했다.

따라서 다음 제품 질문은 더 이상 "structured execution이 가능한가"가 아니다.
현재 증명된 것은 trusted local agent용 non-interactive execution substrate다.
다음 단계는 실제 사용 로그를 바탕으로 PTY/input-required/human handoff 중 어느
하나가 가장 먼저 필요한지 결정하는 것이다. workspace scope는 cwd와 record를
제한하지만 OS sandbox는 아니므로, untrusted agent 격리는 별도 설계 과제다.

## 2026-07 결정: terminal UI가 아니라 execution substrate부터

Loomterm의 첫 제품 경계는 GUI terminal emulator가 아니다. **외부 coding
agent가 화면을 긁지 않고 명령을 실행하고, stdout/stderr, exit code, cwd,
duration을 구조화된 record로 받는 로컬 실행 런타임**이다.

- `loomd`가 process group과 durable event log를 소유한다.
- `loom` CLI와 `loom-mcp`는 동일한 core protocol의 adapter다.
- v0는 pipe 기반 non-interactive execution에 집중한다.
- PTY/TUI snapshot, human handoff, GUI workbench는 같은 execution model 위의
  후속 계층으로 둔다.

이 선택은 2026년의 경쟁 지형도 반영한다. Warp는 shell hook과 command별
grid를 이용한 Blocks data model을 이미 갖고 있고, cmux는 여러 CLI agent를
관리하는 workbench를 제공한다. Loomterm이 먼저 증명할 빈 공간은 또 다른
chat pane이나 tab manager가 아니라 **agent-independent structured process
runtime**이다.

## 이건 실력 격차가 아니라 목표 격차야

Ghostty/WezTerm/Kitty/tmux 만드는 사람들 정말 대단해. 이 연구가 조사한
**네 구현의 범위에서는** 명령 구조를 grid tag로 축약하거나 버리고,
interactive prompt에서만 만들며, exit code가 control API로 이어지지 않았다.
이 결론을 Warp를 포함한 모든 terminal에 일반화해서는 안 된다.

tmux의 `f3c6b4f`와 revert `6fd9987`도 정확히 읽어야 한다. 그 변경은
unbounded command history를 저장한 것이 아니라 pane의 **마지막 command
start/end/status와 event hooks**를 추가했다가 즉시 되돌린 것이다. 따라서
"durable command records를 만들었다가 포기했다"는 직접 증거로 쓰지 않는다.
다만 terminal 내부의 transient command state를 외부 event로 내보내는 설계가
실제로 시도됐다는 근거로는 유효하다.

→ 네가 그들보다 빠른 GPU 렌더러를 만들 필요가 없어. 그건 열린 문이 아니야. 열린 문은 **그들이 최적화하지 않는 다른 문제** — coding agent라는 사용자는 겨우 2년 된 신규 사용자거든. 새 사용자 = 새 제약 = 진짜 greenfield. 여기선 "그들만큼 똑똑한가"가 아니라 "그들이 중심에 안 둔 문제를 신경 쓰는가"가 관건이야.

## "새로 만든다"의 의미를 다시 정의해봐

네가 쓴 `ai-native-terminal.md`가 이미 답을 담고 있어 — **95%는 계승하고, 얇은 레이어만 발명**. 즉:
- Ghostty와 렌더링으로 경쟁 ❌
- VT 스택·Williams 파서·OSC 133·Kitty 프로토콜 전부 물려받고, 그 위에 **semantic command 레이어 + agent API**만 얹기 ⭕

이건 "터미널을 처음부터"가 아니라 **persistent process runtime + local
protocol + CLI/MCP adapter**라는 훨씬 작고 다룰 만한 표면이야. 연구가
보여줬듯 조각들(tmux `%output` push, Kitty schema/security, OSC 133;D의 exit
code)은 이미 존재한다. Loomterm은 prompt marker를 기다리지 않고 자신이
spawn한 process의 boundary와 exit를 직접 소유한다.

## "의미 있는가"도 다시 정의해봐

"널리 쓰이는 제품"을 의미로 잡으면 — 그건 어렵고, 솔직히 아이디어보다 타이밍·유통·운의 문제라 통제 밖이야.

근데 이 저장소를 만들면서 넌 **이 설계 공간을 아주 깊게 이해**하게 됐어. 그 이해가 진짜 희소하고, 코드보다 그게 moat야. 그러니 의미를 이렇게 잡는 게 정직해:
- **한 가지를 증명**하는 프로토타입 — "에이전트가 화면을 긁지 않고 구조화된 명령 결과를 받는다" (도발적 질문 #2: exit code 하나만 retain+push). 이거 하나만 돌아가도 의미 있어.
- 네가 **직접 느끼는 마찰**을 푸는 것 — 넌 coding agent를 쓰는 사람이잖아. 자기가 겪는 friction을 푸는 건 언제나 의미 있고, 사용자 리서치가 공짜야.
- 그 과정에서 배우는 것. 이건 확실한 payoff고 아무도 못 뺏어.

## 정직한 리스크도 말할게

- 이 방향은 **인큐번트도 움직이고 있어**. Warp는 agentic development
  environment로 확장했고, cmux와 ACP 기반 client는 multi-agent workbench를
  빠르게 채우고 있다. 그래서 Loomterm은 model이나 workbench를 소유하지 않고
  실행 계층을 제품 경계로 삼는다.
- 확실한 건 "제품 성공"이 아니라 "돌아가는 프로토타입 + 깊은 이해". 그걸 목표로 시작하면 실패가 없어.

## 그래서, 내 진짜 대답

**의미 있어. 단, "더 나은 터미널"을 만들려 하지 말고 "에이전트를 위한 얇은 레이어"를 만들어.** 대단한 사람들이 안 하는 건 그들이 못해서가 아니라 그들 문제가 아니라서고, 그건 네가 파고들 수 있는 진짜 틈이야.

첫 걸음은 이제 구현되어 있다:
> **`loomd`가 명령을 실행하고 `started/output/finished{exit_code, duration,
> cwd}`를 SQLite와 로컬 socket에 보존하며, CLI와 MCP가 같은 execution id로
> 다시 조회한다. 인간용 terminal은 그대로 둔다.**

다음 검증은 실제 coding agent에 `loom-mcp`를 연결하고, 기존 shell tool 대비
screen parsing, 재접속, long-running command 관찰이 얼마나 단순해지는지
측정하는 것이다.
