----------------------------- MODULE ChannelHunt -----------------------------
(***************************************************************************)
(* Who gets one of MaxChannels channels.                                    *)
(*                                                                         *)
(* The claims are allocation.rs's wants and the allocation its rank        *)
(* prefix; the claimants are call_state.rs's control-channel hunts         *)
(* (tick_hunts), trunked voice calls and conventional channels. Two        *)
(* systems: Local, whose grants are recorded, and Watch, a control_only    *)
(* system followed for its metrics only (watch-cris-diablo).               *)
(*                                                                         *)
(* Current.cfg is the code as of trunk-oxide-86n: every control decoder    *)
(* claims at control_channel_priority (0 by default) and a lost hunt opens *)
(* every candidate. TLC finds the incident: Watch's reception fades for    *)
(* good, its hunt holds the channels, and a voice call waits forever.      *)
(* Fixed.cfg is the fix: Watch's decoders rank below every other claim     *)
(* (Background) and a hunt decodes at most HuntWidth candidates; after     *)
(* HuntDwell ticks without the control channel, the decoders that had a    *)
(* channel close and go to the back of the line, as many as there are      *)
(* candidates waiting, and the next in line open (make_way, fill_hunt).    *)
(*                                                                         *)
(* What TLC found (MaxChannels 3, candidates 2 and 3, 2 calls, 1 conv):    *)
(*  - Current: VoiceServed fails, and even a Watch decoder that is         *)
(*    following its channel keeps a voice call waiting for good.           *)
(*  - Background alone (BackgroundOnly.cfg): VoiceServed holds          *)
(*    but WatchFollows fails. With fewer spare channels than candidates,   *)
(*    the decoders that got one keep it as holders, the rest are never     *)
(*    listened to, and a hunt can sit on the wrong candidate for ever. A   *)
(*    hunt has to close the candidates it has tried and open others.       *)
(*  - Fixed: everything holds (14,000 states; 23,126 with 4 Watch         *)
(*    candidates, 4 channels and 2 conventional channels).                 *)
(*                                                                         *)
(* Abstractions:                                                           *)
(*  - A tick is one system's; the code ticks them all in one step.         *)
(*  - A decoder hears its control channel in a tick iff it holds a channel *)
(*    and its candidate is the one the site carries where we can receive   *)
(*    it (carrying; 0 for Watch is "faded out"). ACQUIRE_MESSAGES is the   *)
(*    "hears" threshold.                                                    *)
(*  - A want's age (`since`) is a fixed order: a hunt's decoders in the    *)
(*    order it opened them, then calls by id, then conventional channels. *)
(*  - A trunked call is one-shot: granted once, then done (ended, evicted  *)
(*    or abandoned when its grants stop while it waits).                   *)
(*                                                                         *)
(* Run (tla2tools.jar from github.com/tlaplus/tlaplus/releases):           *)
(*   java -cp tla2tools.jar tlc2.TLC -config Current.cfg ChannelHunt.tla   *)
(*   java -cp tla2tools.jar tlc2.TLC -config Fixed.cfg ChannelHunt.tla     *)
(*   java -cp tla2tools.jar tlc2.TLC -config BackgroundOnly.cfg ChannelHunt.tla *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences

CONSTANTS
    MaxChannels,    \* [recorder] max_channels
    LocalCands,     \* candidates (configured + announced) of each system
    WatchCands,
    Calls,          \* trunked voice calls on Local
    Convs,          \* conventional channels: standing claims
    ControlPrio,    \* control_channel_priority (lower wins)
    VoicePrio,
    ConvPrio,
    Background,     \* TRUE: Watch's decoders rank below every other claim
    HuntWidth,      \* candidates a hunt opens at once
    HuntDwell,      \* ticks a hunt's window gets to hear one
    LostTicks,      \* LOST_TICKS
    MaxMoves        \* path changes (Fade, Move) in a behaviour

ASSUME MaxChannels \in Nat /\ HuntWidth >= 1 /\ HuntDwell >= 1 /\ LostTicks >= 1
       /\ MaxMoves \in Nat

Local == 1
Watch == 2
Systems == {Local, Watch}
N(s) == IF s = Local THEN LocalCands ELSE WatchCands
Cands(s) == 1..N(s)
Min(a, b) == IF a < b THEN a ELSE b
Max(a, b) == IF a > b THEN a ELSE b
Range(q) == {q[i] : i \in DOMAIN q}
Without(q, gone) == SelectSeq(q, LAMBDA f : f \notin Range(gone))
Index(q, f) == CHOOSE i \in DOMAIN q : q[i] = f

VARIABLES
    carrying,   \* the candidate each site's control channel is on, as received
    calls,      \* each call: "idle", "on" (granted, wants a channel), "done"
    hunt,       \* each system's hunt (ControlHunt): live (0 = hunting), quiet,
                \* dwell, line (the candidates in the order they are tried)
                \* and open (its decoders, each a want, in the order opened)
    held,       \* the wants holding a channel
    moves       \* path changes so far

vars == <<carrying, calls, hunt, held, moves>>

CC(s, f) == <<"cc", s, f>>
Call(c) == <<"call", c, 0>>
Conv(v) == <<"conv", v, 0>>

(* fill_hunt: decode up to HuntWidth candidates, the first in line not    *)
(* decoded yet.                                                            *)
Fill(line, op) ==
    LET rest == Without(line, op)
    IN op \o SubSeq(rest, 1, Min(Max(HuntWidth - Len(op), 0), Len(rest)))

Wants(hs, cs) ==
    UNION {{CC(s, f) : f \in Range(hs[s].open)} : s \in Systems}
    \cup {Call(c) : c \in {x \in Calls : cs[x] = "on"}}
    \cup {Conv(v) : v \in Convs}

Prio(id) == CASE id[1] = "cc"   -> ControlPrio
              [] id[1] = "call" -> VoicePrio
              [] id[1] = "conv" -> ConvPrio

Bg(id) == Background /\ id[1] = "cc" /\ id[2] = Watch

\* The age order standing in for `since`, then id (lower is older).
Key(id, hs) == CASE id[1] = "cc"   -> 100 * id[2] + Index(hs[id[2]].open, id[3])
                 [] id[1] = "call" -> 1000 + id[2]
                 [] id[1] = "conv" -> 2000 + id[2]

(* allocation::Rank: tier, priority, holder before newcomer, age.          *)
RankOf(id, h, hs) ==
    <<IF Bg(id) THEN 1 ELSE 0, Prio(id), IF id \in h THEN 0 ELSE 1, Key(id, hs)>>

Before(x, y) ==
    \/ x[1] < y[1]
    \/ x[1] = y[1] /\ x[2] < y[2]
    \/ x[1] = y[1] /\ x[2] = y[2] /\ x[3] < y[3]
    \/ x[1] = y[1] /\ x[2] = y[2] /\ x[3] = y[3] /\ x[4] < y[4]

(* allocation::allocate: the first n of S in rank order, given holders h. *)
RECURSIVE Take(_, _, _, _)
Take(S, n, h, hs) ==
    IF n = 0 \/ S = {} THEN {}
    ELSE LET m == CHOOSE x \in S :
                    \A y \in S \ {x} : Before(RankOf(x, h, hs), RankOf(y, h, hs))
         IN {m} \cup Take(S \ {m}, n - 1, h, hs)

(* settle(): the claims after a step are cs0/hs1; the allocation          *)
(* follows. A trunked call that falls out is evicted (done); a control     *)
(* decoder or conventional channel that falls out keeps its claim. Its     *)
(* value is the next state of the claims, as a record.                     *)
Settle(cs0, hs1) ==
    LET W  == Wants(hs1, cs0)
        H0 == held \cap W
        H1 == Take(W, MaxChannels, H0, hs1)
        ev == {c \in Calls : Call(c) \in H0 \ H1}
    IN [held  |-> H1,
        calls |-> [c \in Calls |-> IF c \in ev THEN "done" ELSE cs0[c]],
        hunt  |-> hs1]

Claims == [held |-> held, calls |-> calls, hunt |-> hunt]

\* Take the claims to r; the paths stay as they are.
Become(r) ==
    /\ held' = r.held
    /\ calls' = r.calls
    /\ hunt' = r.hunt
    /\ UNCHANGED <<carrying, moves>>

-----------------------------------------------------------------------------

Init ==
    LET cs == [c \in Calls |-> "idle"]
        line(s) == [i \in Cands(s) |-> i]
        hs == [s \in Systems |-> [live |-> 0, quiet |-> 0, dwell |-> 0,
                                  line |-> line(s), open |-> Fill(line(s), <<>>)]]
    IN /\ carrying \in {r \in [Systems -> 0..(LocalCands + WatchCands)] :
                           r[Local] \in Cands(Local) /\ r[Watch] \in 0..N(Watch)}
       /\ calls = cs
       /\ hunt = hs
       /\ held = Take(Wants(hs, cs), MaxChannels, {}, hs)
       /\ moves = 0

\* A grant is heard only while Local's control channel is followed.
Grant(c) ==
    /\ calls[c] = "idle"
    /\ hunt[Local].live # 0
    /\ Become(Settle([calls EXCEPT ![c] = "on"], hunt))

\* A recorded call ends.
End(c) ==
    /\ calls[c] = "on"
    /\ Call(c) \in held
    /\ Become(Settle([calls EXCEPT ![c] = "done"], hunt))

\* A waiting call's grants stop and it times out (nothing makes this happen).
Abandon(c) ==
    /\ calls[c] = "on"
    /\ Call(c) \notin held
    /\ Become(Settle([calls EXCEPT ![c] = "done"], hunt))

\* Watch's path changes: its control channel moves, or fades out (0).
Fade(f) ==
    /\ f # carrying[Watch]
    /\ moves < MaxMoves
    /\ moves' = moves + 1
    /\ carrying' = [carrying EXCEPT ![Watch] = f]
    /\ UNCHANGED <<calls, hunt, held>>

\* Local's site moves its control channel to another candidate.
Move(f) ==
    /\ f # carrying[Local]
    /\ moves < MaxMoves
    /\ moves' = moves + 1
    /\ carrying' = [carrying EXCEPT ![Local] = f]
    /\ UNCHANGED <<calls, hunt, held>>

(* make_way: as many of the decoders that had a channel as there are      *)
(* candidates waiting their turn close, oldest first, and go to the back   *)
(* of the line; the next in line open.                                     *)
MakeWay(s, h) ==
    LET waiting == Len(h.line) - Len(h.open)
        had     == SelectSeq(h.open, LAMBDA f : CC(s, f) \in held)
        tried   == SubSeq(had, 1, Min(waiting, Len(had)))
        line    == Without(h.line, tried) \o tried
    IN [h EXCEPT !.line = line, !.open = Fill(line, Without(h.open, tried)), !.dwell = 0]

(* tick_hunts for system s. Hunting: take the candidate that heard, close *)
(* the rest; after HuntDwell ticks without one, make way. Following: lost  *)
(* after LostTicks silent ticks, and the hunt fills up again.              *)
TickTo(s) ==
    LET h     == hunt[s]
        hears == CC(s, carrying[s]) \in held
    IN IF h.live = 0 THEN
         IF hears THEN
           Settle(calls, [hunt EXCEPT ![s] = [h EXCEPT !.live = carrying[s], !.quiet = 0,
                                                       !.dwell = 0, !.open = <<carrying[s]>>]])
         ELSE IF h.dwell + 1 >= HuntDwell THEN
           Settle(calls, [hunt EXCEPT ![s] = MakeWay(s, h)])
         ELSE
           Settle(calls, [hunt EXCEPT ![s] = [h EXCEPT !.dwell = h.dwell + 1]])
       ELSE
         IF hears THEN
           Settle(calls, [hunt EXCEPT ![s] = [h EXCEPT !.quiet = 0]])
         ELSE IF h.quiet + 1 >= LostTicks THEN
           Settle(calls, [hunt EXCEPT ![s] = [h EXCEPT !.live = 0, !.quiet = 0,
                                                       !.open = Fill(h.line, h.open)]])
         ELSE
           Settle(calls, [hunt EXCEPT ![s] = [h EXCEPT !.quiet = h.quiet + 1]])

Tick(s) == Become(TickTo(s))

Next ==
    \/ \E c \in Calls : Grant(c) \/ End(c) \/ Abandon(c)
    \/ \E s \in Systems : Tick(s)
    \/ \E f \in 0..N(Watch) : Fade(f)
    \/ \E f \in Cands(Local) : Move(f)

\* Time passes and recorded calls end; the radio paths and the talkgroups
\* owe us nothing, except that the paths change at most MaxMoves times: the
\* last change (Watch fading out, say) then lasts for ever.
\*
\* Weak fairness written out, WF_vars(A) being <>[]ENABLED <<A>>_vars =>
\* []<><<A>>_vars, with ENABLED as the state predicate it amounts to: TLC
\* evaluating ENABLED itself re-runs the allocator many times a state.
TickChanges(s) == TickTo(s) # Claims
CanEnd(c) == calls[c] = "on" /\ Call(c) \in held

Fairness ==
    /\ \A s \in Systems : <>[]TickChanges(s) => []<><<Tick(s)>>_vars
    /\ \A c \in Calls : <>[]CanEnd(c) => []<><<End(c)>>_vars

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------

TypeOK ==
    /\ carrying[Local] \in Cands(Local) /\ carrying[Watch] \in 0..N(Watch)
    /\ calls \in [Calls -> {"idle", "on", "done"}]
    /\ moves \in 0..MaxMoves
    /\ \A s \in Systems :
         /\ Len(hunt[s].line) = N(s) /\ Range(hunt[s].line) = Cands(s)
         /\ Range(hunt[s].open) \subseteq Cands(s)
         /\ Cardinality(Range(hunt[s].open)) = Len(hunt[s].open)
         /\ hunt[s].live \in 0..N(s)
         /\ hunt[s].live # 0 => hunt[s].open = <<hunt[s].live>>
    /\ held \subseteq Wants(hunt, calls)

WithinBudget == Cardinality(held) <= MaxChannels

\* A rank prefix: nothing waiting outranks anything holding.
IsPrefix ==
    \A w \in Wants(hunt, calls) \ held, x \in held :
        Before(RankOf(x, held, hunt), RankOf(w, held, hunt))

IsWatch(id) == id[1] = "cc" /\ id[2] = Watch

\* The incident as an invariant: a watch decoder holds a channel while a
\* claim that records (or Local's own control channel) waits.
WatchNeverBlocks ==
    (\E x \in held : IsWatch(x)) => \A w \in Wants(hunt, calls) \ held : IsWatch(w)

\* A hunt decodes at most HuntWidth candidates.
HuntWithinWidth == \A s \in Systems : Len(hunt[s].open) <= HuntWidth

\* A granted call gets a channel or ends: Watch's reception, whatever it
\* does, cannot keep it waiting.
VoiceServed ==
    \A c \in Calls : (calls[c] = "on") ~> (Call(c) \in held \/ calls[c] = "done")

\* The cap does not stop a hunt finding its channel: once Watch is received
\* for good, it ends up following it.
WatchFollows ==
    <>[](carrying[Watch] # 0) => <>[](hunt[Watch].live = carrying[Watch])

=============================================================================
