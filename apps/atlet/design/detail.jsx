// detail.jsx — three timer detail views: time (countdown), reps (tap-to-log), distance (pace + map)
// Plus a shared shell with header, step strip, controls.

const useTicker = (running, fps = 30) => {
  const [, force] = React.useState(0);
  React.useEffect(() => {
    if (!running) return;
    const id = setInterval(() => force(t => t + 1), 1000 / fps);
    return () => clearInterval(id);
  }, [running, fps]);
};

const beep = (freq = 880, dur = 0.08, vol = 0.05) => {
  try {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    if (!Ctx || !window.__audioOn) return;
    const ctx = window.__audioCtx || (window.__audioCtx = new Ctx());
    const o = ctx.createOscillator(), g = ctx.createGain();
    o.frequency.value = freq;
    o.type = 'sine';
    g.gain.value = vol;
    o.connect(g); g.connect(ctx.destination);
    o.start();
    g.gain.exponentialRampToValueAtTime(0.0001, ctx.currentTime + dur);
    o.stop(ctx.currentTime + dur);
  } catch {}
};

const Dial = ({ progress = 0, urgent, children }) => {
  const r = 130, c = 2 * Math.PI * r;
  const dash = c * (1 - progress);
  // Theme flows through tokens only — no JS-side dark branching. The track is a
  // hairline on the app surface (--rule), the foreground is the accent when
  // urgent else primary text. (GLM-5.2: killing the dark prop makes the
  // countdown dial correct in light AND dark AND auto.)
  const trackColor = 'var(--rule)';
  const fgColor = urgent ? 'var(--accent)' : 'var(--ink)';
  return (
    <div className="dial-wrap">
      <svg className="dial-bg" viewBox="0 0 290 290">
        <circle cx="145" cy="145" r={r} stroke={trackColor} strokeWidth="6" fill="none"/>
      </svg>
      <svg className="dial-fg" viewBox="0 0 290 290">
        <circle cx="145" cy="145" r={r} stroke={fgColor} strokeWidth="6" fill="none"
          strokeLinecap="round"
          strokeDasharray={c}
          strokeDashoffset={dash}
          style={{transition:'stroke-dashoffset 0.2s linear, stroke 0.2s'}}
        />
      </svg>
      {children}
    </div>
  );
};

const stepMeta = (s) => {
  if (s.dur != null) return `${s.dur}s`;
  if (s.reps != null) return `${s.reps} reps`;
  if (s.km != null) return `${s.km} km`;
  return '';
};

const UpNext = ({ steps, currentIdx, isLast, restDur = 0 }) => {
  if (!steps || steps.length === 0) return null;
  const nxt = !isLast ? steps[currentIdx + 1] : null;
  return (
    <div className="upnext rise" style={{animationDelay:'180ms'}}>
      <div className="lbl">Up next</div>
      <div className="name">
        {nxt ? (
          <>
            <div style={{display:'flex', alignItems:'center', gap:8, flexWrap:'wrap'}}>
              {restDur > 0 && <span className="rest-pill"><span className="dot"/>{restDur}s rest</span>}
              <span>{nxt.name}</span>
            </div>
            <div className="meta">{stepMeta(nxt)} · {currentIdx + 2}/{steps.length}</div>
          </>
        ) : (
          <>
            <div>Final step</div>
            <div className="meta">Push through — last one</div>
          </>
        )}
      </div>
      <div className="arrow"><Icon name="back" size={12} style={{transform:'rotate(180deg)'}}/></div>
    </div>
  );
};

const StepStrip = ({ steps, currentIdx, currentProgress, restDur = 0 }) => (
  <div className="step-strip">
    {steps.map((_, i) => (
      <React.Fragment key={i}>
        <div className={`step-pip ${i < currentIdx ? 'done' : ''}`}>
          <div style={{width: i === currentIdx ? `${currentProgress * 100}%` : (i < currentIdx ? '100%' : '0%')}}/>
        </div>
        {restDur > 0 && i < steps.length - 1 && (
          <div className={`step-rest ${i < currentIdx ? 'done' : ''}`} title={`${restDur}s rest`}/>
        )}
      </React.Fragment>
    ))}
  </div>
);

// === REST INTERLUDE (between-step countdown, shared by all detail views) ===
const RestInterlude = ({ dur, onSkip, onEdit, nextLabel }) => {
  const [remaining, setRemaining] = React.useState(dur);
  const startRef = React.useRef(performance.now());
  const durRef = React.useRef(dur);

  React.useEffect(() => {
    // allow live-edit to extend/reduce: reset timing baseline when dur changes
    const elapsed = (performance.now() - startRef.current) / 1000;
    const prevDur = durRef.current;
    const used = prevDur - remaining;
    startRef.current = performance.now() - used * 1000;
    durRef.current = dur;
  }, [dur]);

  React.useEffect(() => {
    const id = setInterval(() => {
      const elapsed = (performance.now() - startRef.current) / 1000;
      const r = Math.max(0, durRef.current - elapsed);
      setRemaining(r);
      if (r <= 0) { clearInterval(id); onSkip(); }
      else if (r <= 3 && r > 0) {
        const last = window.__restTick;
        const cur = Math.ceil(r);
        if (last !== cur) { window.__restTick = cur; beep(660, 0.05, 0.04); }
      }
    }, 60);
    return () => clearInterval(id);
  }, []);

  const progress = 1 - remaining / durRef.current;
  const secs = Math.ceil(remaining);
  // Theme via tokens only — the rest card sits on the page surface like every
  // other screen, not on an inverted dark panel. (was: dark? --bone/--ink ternaries)
  const ink = 'var(--ink)';
  const sub = 'var(--ink-3)';
  const ring = 'var(--rule)';
  const bg = 'var(--bone-2)';

  return (
    <div className="rest-interlude" style={{background: bg, color: ink}}>
      <div className="eyebrow" style={{color: sub, textAlign:'center'}}>Rest</div>
      <div style={{position:'relative', width:128, height:128, margin:'0 auto'}}>
        <svg viewBox="0 0 128 128" style={{position:'absolute', inset:0, transform:'rotate(-90deg)'}}>
          <circle cx="64" cy="64" r="58" stroke={ring} strokeWidth="4" fill="none"/>
          <circle cx="64" cy="64" r="58" stroke="var(--accent)" strokeWidth="4" fill="none"
            strokeLinecap="round"
            strokeDasharray={2*Math.PI*58}
            strokeDashoffset={2*Math.PI*58 * progress}
            style={{transition:'stroke-dashoffset 0.1s linear'}}/>
        </svg>
        <div style={{position:'absolute', inset:0, display:'flex', flexDirection:'column', alignItems:'center', justifyContent:'center'}}>
          <div className="num" style={{fontSize:42, fontWeight:600, color: ink, lineHeight:1}}>{secs}</div>
          <div className="caption" style={{fontSize:9, color: sub, marginTop:2, letterSpacing:'0.14em', textTransform:'uppercase'}}>sec</div>
        </div>
      </div>
      <div className="caption" style={{textAlign:'center', color: sub, fontSize:12}}>
        Up next · <span style={{color: ink, fontWeight:600}}>{nextLabel}</span>
      </div>
      <div className="rest-controls">
        <button className="rest-btn" onClick={() => onEdit(Math.max(5, durRef.current - 15))}>−15s</button>
        <button className="rest-btn primary" onClick={onSkip}>Skip</button>
        <button className="rest-btn" onClick={() => onEdit(durRef.current + 15)}>+15s</button>
      </div>
    </div>
  );
};

// === FEEDBACK SHEET (post-workout rating) ===
const FEEDBACK_OPTIONS = [
  { key: 'tough',   emoji: '\ud83e\udd75', label: 'Brutal' },
  { key: 'hard',    emoji: '\ud83d\ude24', label: 'Hard' },
  { key: 'solid',   emoji: '\ud83d\ude42', label: 'Solid' },
  { key: 'easy',    emoji: '\ud83d\ude0e', label: 'Easy' },
  { key: 'amazing', emoji: '\ud83d\udd25', label: 'On fire' },
];
const FEEDBACK_TAGS = ['Felt strong', 'Short on time', 'Sore', 'Good form', 'Crushed it', 'Low energy', 'New PR'];

const FeedbackSheet = ({ open, summary, onSkip, onSubmit }) => {
  const [rating, setRating] = React.useState(null);
  const [tags, setTags] = React.useState([]);
  const [note, setNote] = React.useState('');

  React.useEffect(() => {
    if (open) { setRating(null); setTags([]); setNote(''); }
  }, [open]);

  const toggleTag = (t) => setTags(ts => ts.includes(t) ? ts.filter(x => x !== t) : [...ts, t]);
  const submit = () => onSubmit({ rating, tags, note });

  return (
    <>
      <div className={`fb-scrim ${open ? 'open' : ''}`} onClick={onSkip}/>
      <div className={`fb-sheet ${open ? 'open' : ''}`}>
        <div className="fb-grab"/>
        <div className="fb-head">
          <div className="eyebrow">Workout complete</div>
          <div className="h-2">How did it go?</div>
          <div className="caption num" style={{marginTop:4, color:'var(--ink-3)'}}>{summary}</div>
        </div>

        <div className="fb-emoji-row">
          {FEEDBACK_OPTIONS.map(o => (
            <button
              key={o.key}
              className={`fb-emoji ${rating === o.key ? 'active' : ''}`}
              onClick={() => setRating(o.key)}
              aria-label={o.label}
            >
              <span className="glyph">{o.emoji}</span>
              <span className="lbl">{o.label}</span>
            </button>
          ))}
        </div>

        <div className={`fb-tags-wrap ${rating ? 'shown' : ''}`}>
          <div className="fb-section-label">Tags <span className="opt">optional</span></div>
          <div className="fb-tags">
            {FEEDBACK_TAGS.map(t => (
              <button
                key={t}
                className={`fb-tag ${tags.includes(t) ? 'active' : ''}`}
                onClick={() => toggleTag(t)}
              >
                {t}
              </button>
            ))}
          </div>

          <div className="fb-section-label">Note <span className="opt">optional</span></div>
          <textarea
            className="fb-note"
            placeholder="Add a note for your journal\u2026"
            value={note}
            onChange={e => setNote(e.target.value)}
            maxLength={200}
          />
        </div>

        <div className="fb-actions">
          <button className="fb-btn ghost" onClick={onSkip}>Skip</button>
          <button className="fb-btn primary" onClick={submit} disabled={!rating}>
            Log workout
          </button>
        </div>
      </div>
    </>
  );
};

const useFeedback = (done, onBack) => {
  const [open, setOpen] = React.useState(false);
  React.useEffect(() => {
    if (done) {
      const t = setTimeout(() => setOpen(true), 700);
      return () => clearTimeout(t);
    } else {
      setOpen(false);
    }
  }, [done]);
  const close = () => {
    setOpen(false);
    setTimeout(() => onBack(), 320);
  };
  return { open, close };
};

// === TIME-BASED (countdown) ===
const TimeDetail = ({ w, onBack, onEdit }) => {
  const [running, setRunning] = React.useState(false);
  const [elapsed, setElapsed] = React.useState(0);
  const [stepIdx, setStepIdx] = React.useState(0);
  const [done, setDone] = React.useState(false);
  const [resting, setResting] = React.useState(false);
  const [restDur, setRestDur] = React.useState(w.restDur ?? 0); // Tabata defaults to 0
  const startRef = React.useRef(0);
  const baseRef = React.useRef(0);
  const [flashHalf, setFlashHalf] = React.useState(false);
  const [tickPulse, setTickPulse] = React.useState(0);

  const isMulti = w.structure === 'multi' && w.steps;
  const currentStep = isMulti ? w.steps[stepIdx] : null;
  const stepDur = isMulti ? currentStep.dur : w.target;
  const totalDur = isMulti ? w.steps.reduce((a, s) => a + s.dur, 0) : w.target;

  useTicker(running && !resting, 30);

  React.useEffect(() => {
    if (!running || done || resting) return;
    const t = (performance.now() - startRef.current) / 1000 + baseRef.current;
    setElapsed(t);
    const remaining = stepDur - t;
    if (remaining <= 0) {
      // step complete
      beep(880);
      if (isMulti && stepIdx < w.steps.length - 1) {
        if (restDur > 0) {
          setResting(true);
        } else {
          baseRef.current = 0;
          startRef.current = performance.now();
          setElapsed(0);
          setStepIdx(stepIdx + 1);
        }
      } else {
        setRunning(false);
        setDone(true);
        beep(1320, 0.2, 0.08);
        setTimeout(() => beep(1760, 0.2, 0.08), 180);
      }
    }
  });

  const advanceStep = () => {
    setResting(false);
    baseRef.current = 0;
    startRef.current = performance.now();
    setElapsed(0);
    setStepIdx(i => i + 1);
  };

  const goToStep = (idx) => {
    if (!isMulti || idx < 0 || idx >= w.steps.length) return;
    setStepIdx(idx);
    setResting(false);
    setElapsed(0);
    baseRef.current = 0;
    startRef.current = performance.now();
    setDone(false);
  };

  // halfway and final-10 cues
  React.useEffect(() => {
    if (!running) return;
    const remaining = Math.ceil(stepDur - elapsed);
    if (remaining === Math.floor(stepDur / 2) && stepDur > 20) {
      setFlashHalf(true);
      setTimeout(() => setFlashHalf(false), 600);
    }
    if (remaining <= 10 && remaining > 0) {
      const lastTick = window.__lastTick;
      if (lastTick !== remaining) {
        window.__lastTick = remaining;
        beep(660, 0.05, 0.04);
        setTickPulse(t => t + 1);
      }
    }
  }, [elapsed, running, stepDur]);

  const remaining = Math.max(0, stepDur - elapsed);
  const progress = elapsed / stepDur;
  const urgent = remaining <= 10 && remaining > 0 && running;

  const start = () => {
    if (done) {
      setDone(false); setStepIdx(0); setElapsed(0); baseRef.current = 0;
    }
    startRef.current = performance.now();
    setRunning(true);
  };
  const pause = () => {
    baseRef.current += (performance.now() - startRef.current) / 1000;
    setRunning(false);
  };
  const reset = () => {
    setRunning(false); setElapsed(0); setStepIdx(0); setDone(false); baseRef.current = 0;
  };

  const fb = useFeedback(done, onBack);
  const summary = isMulti
    ? `${w.steps.length} steps · ${fmtSec(totalDur)}`
    : fmtSec(totalDur);
  return (
    <>
    <div className={`detail fade ${urgent ? 'urgent' : ''}`}>
      <div className={`flash-overlay ${flashHalf ? 'active' : ''}`}/>
      <div className="detail-header">
        <button className="icon-btn" onClick={onBack}><Icon name="back" size={18}/></button>
        <div className="eyebrow">{resting ? 'Rest' : 'Countdown'}</div>
        <button className="icon-btn" onClick={onEdit}><Icon name="edit" size={16}/></button>
      </div>

      <div className="detail-intro">
        <div className="h-1 rise" style={{animationDelay:'40ms'}}>{w.name}</div>
        <div className="caption rise" style={{animationDelay:'100ms'}}>
          {isMulti ? `Step ${stepIdx + 1} of ${w.steps.length} · ${currentStep.name}` : w.notes}
        </div>
      </div>

      {isMulti && <StepStrip steps={w.steps} currentIdx={stepIdx} currentProgress={progress} restDur={restDur}/>}
      {isMulti && !resting && <UpNext steps={w.steps} currentIdx={stepIdx} isLast={stepIdx === w.steps.length - 1} restDur={restDur}/>}

      {resting ? (
        <RestInterlude
          dur={restDur}
          nextLabel={w.steps[stepIdx + 1]?.name || 'Next step'}
          onSkip={advanceStep}
          onEdit={setRestDur}
        />
      ) : (
      <div className="timer-stage">
        <Dial progress={progress} urgent={urgent}>
          <div className="dial-readout" key={tickPulse} style={{animation: urgent ? 'none' : 'flashIn 0.4s'}}>
            <div className="big" style={{color: urgent ? 'var(--accent)' : 'var(--ink)'}}>{fmtSec(remaining)}</div>
            <div className="label">{done ? 'Complete' : (running ? 'Remaining' : 'Paused')}</div>
          </div>
        </Dial>

        {done && (
          <div className="flash-complete" style={{display:'flex', flexDirection:'column', alignItems:'center', gap:8}}>
            <div style={{width:54, height:54, borderRadius:'50%', background:'var(--accent)', display:'flex', alignItems:'center', justifyContent:'center', color:'var(--text-on-accent)'}}>
              <Icon name="check" size={28}/>
            </div>
            <div className="caption" style={{fontSize:13, fontWeight:500}}>
              {fmtSec(totalDur)} logged · nice work
            </div>
          </div>
        )}
      </div>
      )}

      <div className="controls" style={{display: resting ? 'none' : undefined}}>
        {isMulti && (
          <button className="step-nav-btn" onClick={() => goToStep(stepIdx - 1)} disabled={stepIdx === 0}>
            <Icon name="back" size={16}/>
          </button>
        )}
        <button className="ctrl" onClick={reset}><Icon name="reset" size={20}/></button>
        <button className="ctrl primary" onClick={running ? pause : start}>
          <Icon name={running ? 'pause' : 'play'} size={28}/>
        </button>
        <button className="ctrl" onClick={() => { window.__audioOn = !window.__audioOn; force(); }}>
          <Icon name="sound" size={20} style={{opacity: window.__audioOn ? 1 : 0.4}}/>
        </button>
        {isMulti && (
          <button className="step-nav-btn" onClick={() => goToStep(stepIdx + 1)} disabled={stepIdx >= w.steps.length - 1}>
            <Icon name="back" size={16} style={{transform:'rotate(180deg)'}}/>
          </button>
        )}
      </div>
    </div>
    <FeedbackSheet open={fb.open} summary={summary} onSkip={fb.close} onSubmit={fb.close}/>
    </>
  );
};
const force = () => {}; // noop fallback

// === REPS (countdown with tempo) ===
const RepsDetail = ({ w, onBack, onEdit }) => {
  const [running, setRunning] = React.useState(false);
  const [repsDone, setRepsDone] = React.useState(0);
  const [stepIdx, setStepIdx] = React.useState(0);
  const [done, setDone] = React.useState(false);
  const [pulseKey, setPulseKey] = React.useState(0);
  const [tempo, setTempo] = React.useState(w.tempo || 1); // live-editable seconds per rep
  const [resting, setResting] = React.useState(false);
  const [restDur, setRestDur] = React.useState(w.restDur ?? 45);
  const startRef = React.useRef(0);
  const baseRef = React.useRef(0);
  const lastTempoRepRef = React.useRef(0);

  const isMulti = w.structure === 'multi' && w.steps;
  const currentStep = isMulti ? w.steps[stepIdx] : null;
  const stepTarget = isMulti ? currentStep.reps : w.target;
  const totalTarget = isMulti ? w.steps.reduce((a, s) => a + s.reps, 0) : w.target;
  const totalDone = isMulti ? w.steps.slice(0, stepIdx).reduce((a,s)=>a+s.reps,0) + repsDone : repsDone;
  const remaining = stepTarget - repsDone;

  const elapsed = running
    ? (performance.now() - startRef.current) / 1000 + baseRef.current
    : baseRef.current;
  const tempoProgress = (elapsed % tempo) / tempo;

  useTicker(running && !resting, 30);

  React.useEffect(() => {
    if (!running || done || resting) return;
    const expected = Math.floor(elapsed / tempo);
    if (expected > lastTempoRepRef.current) {
      lastTempoRepRef.current = expected;
      setRepsDone(prev => {
        const next = prev + 1;
        setPulseKey(k => k + 1);
        beep(660, 0.04, 0.04);
        if (next >= stepTarget) {
          queueMicrotask(() => completeStep(next));
          return next;
        }
        return next;
      });
    }
  });

  const goToStep = (idx) => {
    if (!isMulti || idx < 0 || idx >= w.steps.length) return;
    setStepIdx(idx);
    setRepsDone(0);
    setResting(false);
    lastTempoRepRef.current = 0;
    baseRef.current = 0;
    startRef.current = performance.now();
    setDone(false);
  };

  const completeStep = (finalReps) => {
    if (isMulti && stepIdx < w.steps.length - 1) {
      beep(880, 0.1, 0.06);
      if (restDur > 0) {
        setResting(true);
      } else {
        advanceStep();
      }
    } else {
      setRunning(false);
      setDone(true);
      beep(1320, 0.2, 0.08);
      setTimeout(() => beep(1760, 0.2, 0.08), 180);
    }
  };

  const advanceStep = () => {
    setResting(false);
    setRepsDone(0);
    setStepIdx(i => i + 1);
    lastTempoRepRef.current = 0;
    baseRef.current = 0;
    startRef.current = performance.now();
  };

  const tap = () => { return; };

  const start = () => {
    if (done) {
      setDone(false); setRepsDone(0); setStepIdx(0); baseRef.current = 0;
    }
    if (!running) {
      startRef.current = performance.now();
      lastTempoRepRef.current = Math.floor(baseRef.current / tempo);
    }
    setRunning(true);
  };
  const pause = () => {
    baseRef.current += (performance.now() - startRef.current) / 1000;
    setRunning(false);
  };
  const reset = () => {
    setRunning(false); setRepsDone(0); setStepIdx(0); setDone(false); baseRef.current = 0;
    lastTempoRepRef.current = 0; setResting(false);
  };

  const changeTempo = (t) => {
    setTempo(t);
    lastTempoRepRef.current = Math.floor(elapsed / t);
  };
  const startWithTempo = (t) => {
    changeTempo(t);
    if (!running) start();
  };

  const fb = useFeedback(done, onBack);
  const summary = isMulti
    ? `${totalDone}/${totalTarget} reps · ${w.steps.length} sets`
    : `${repsDone}/${w.target} reps`;
  return (
    <>
    <div className="detail fade">
      <div className="detail-header">
        <button className="icon-btn" onClick={onBack}><Icon name="back" size={18}/></button>
        <div className="eyebrow">{resting ? 'Rest' : `Tempo · ${tempo}s/rep`}</div>
        <button className="icon-btn" onClick={onEdit}><Icon name="edit" size={16}/></button>
      </div>

      <div className="detail-intro">
        <div className="h-1 rise" style={{animationDelay:'40ms'}}>{w.name}</div>
        <div className="caption rise" style={{animationDelay:'100ms'}}>
          {isMulti ? `${currentStep.name} · ${stepIdx+1}/${w.steps.length}` : w.notes}
        </div>
      </div>

      {isMulti && <StepStrip steps={w.steps} currentIdx={stepIdx} currentProgress={repsDone / stepTarget} restDur={restDur}/>}
      {isMulti && !resting && <UpNext steps={w.steps} currentIdx={stepIdx} isLast={stepIdx === w.steps.length - 1} restDur={restDur}/>}

      {resting ? (
        <RestInterlude
          dur={restDur}
          nextLabel={w.steps[stepIdx + 1]?.name || 'Next step'}
          onSkip={advanceStep}
          onEdit={setRestDur}
        />
      ) : (
      <div className="timer-stage" style={{gap:12}}>
        <div style={{position:'relative', width:140, height:140, flexShrink:0}}>
          {/* tempo ring */}
          <svg viewBox="0 0 140 140" style={{position:'absolute', inset:0, transform:'rotate(-90deg)'}}>
            <circle cx="70" cy="70" r="66" stroke="var(--bone-2)" strokeWidth="3" fill="none"/>
            <circle cx="70" cy="70" r="66" stroke="var(--accent)" strokeWidth="3" fill="none"
              strokeLinecap="round"
              strokeDasharray={2*Math.PI*66}
              strokeDashoffset={2*Math.PI*66 * (1 - tempoProgress)}
              style={{transition:'stroke-dashoffset 0.05s linear'}}/>
          </svg>
          <button className="tap-target" onClick={tap} key={pulseKey} style={{
            animation: pulseKey > 0 ? 'flashIn 0.3s cubic-bezier(.2,.85,.2,1)' : 'none',
            width:130, height:130, position:'absolute', inset:5,
            cursor: 'default'
          }}>
            {done ? (
              <>
                <Icon name="check" size={44}/>
                <div className="lbl" style={{marginTop:4, fontSize:9}}>Done</div>
              </>
            ) : (
              <>
                <div className="num" style={{fontSize:38}}>{remaining}</div>
                <div className="lbl" style={{fontSize:9}}>
                  {!running ? 'press start' : 'reps to go'}
                </div>
              </>
            )}
          </button>
        </div>
        <div style={{display:'flex', justifyContent:'space-between', width:'100%', maxWidth:280}}>
          <div style={{display:'flex', flexDirection:'column', gap:2}}>
            <div className="caption" style={{fontSize:9, letterSpacing:'0.14em', textTransform:'uppercase'}}>Time</div>
            <div className="num" style={{fontSize:18, fontWeight:600}}>{fmtSec(elapsed)}</div>
          </div>
          <div style={{display:'flex', flexDirection:'column', gap:2, textAlign:'center'}}>
            <div className="caption" style={{fontSize:9, letterSpacing:'0.14em', textTransform:'uppercase'}}>Done</div>
            <div className="num" style={{fontSize:18, fontWeight:600}}>{repsDone}<span style={{color:'var(--ink-4)'}}>/{stepTarget}</span></div>
          </div>
          <div style={{display:'flex', flexDirection:'column', gap:2, textAlign:'right'}}>
            <div className="caption" style={{fontSize:9, letterSpacing:'0.14em', textTransform:'uppercase'}}>Total</div>
            <div className="num" style={{fontSize:18, fontWeight:600}}>{totalDone}<span style={{color:'var(--ink-4)'}}>/{totalTarget}</span></div>
          </div>
        </div>
      </div>
      )}

      <div className="controls" style={{display: resting ? 'none' : undefined}}>
        {isMulti && (
          <button className="step-nav-btn" onClick={() => goToStep(stepIdx - 1)} disabled={stepIdx === 0}>
            <Icon name="back" size={16}/>
          </button>
        )}
        <button className="ctrl" onClick={reset}><Icon name="reset" size={20}/></button>
        <button className="ctrl primary" onClick={running ? pause : start}>
          <Icon name={running ? 'pause' : 'play'} size={28}/>
        </button>
        <button className="ctrl" onClick={() => { window.__audioOn = !window.__audioOn; setPulseKey(k=>k+1); }}>
          <Icon name="sound" size={20} style={{opacity: window.__audioOn ? 1 : 0.4}}/>
        </button>
        {isMulti && (
          <button className="step-nav-btn" onClick={() => goToStep(stepIdx + 1)} disabled={stepIdx >= w.steps.length - 1}>
            <Icon name="back" size={16} style={{transform:'rotate(180deg)'}}/>
          </button>
        )}
      </div>

      {/* live tempo strip — tap to change pace during or before the workout */}
      <div className="tempo-strip">
        <div className="tempo-strip-label">Tempo</div>
        <div className="tempo-chips">
          {[1, 2, 3, 4, 5, 6].map(v => (
            <button key={v}
              className={`tempo-chip ${tempo === v ? 'active' : ''}`}
              onClick={() => running ? changeTempo(v) : startWithTempo(v)}>
              {v}s
            </button>
          ))}
        </div>
      </div>
    </div>
    <FeedbackSheet open={fb.open} summary={summary} onSkip={fb.close} onSubmit={fb.close}/>
    </>
  );
};

// === DISTANCE (stopwatch + pace + splits) ===
const DistanceDetail = ({ w, onBack, onEdit }) => {
  const [running, setRunning] = React.useState(false);
  const [elapsed, setElapsed] = React.useState(0);
  const [done, setDone] = React.useState(false);
  const [splits, setSplits] = React.useState([]); // [{km, time, pace}]
  const startRef = React.useRef(0);
  const baseRef = React.useRef(0);
  const lastSplitRef = React.useRef(0);

  // simulate progress: 1 km roughly per ~5:30 min @ 5.5 px/sec mock
  const SIM_PACE = 330; // sec per km (sim)
  const km = elapsed / SIM_PACE;
  const targetKm = w.target;
  const progress = Math.min(1, km / targetKm);

  useTicker(running, 30);

  React.useEffect(() => {
    if (!running || done) return;
    const t = (performance.now() - startRef.current) / 1000 + baseRef.current;
    setElapsed(t);
    // log split
    const newKm = Math.floor(t / SIM_PACE);
    if (newKm > splits.length && newKm <= targetKm) {
      const splitTime = t - lastSplitRef.current;
      lastSplitRef.current = t;
      const paceMin = Math.floor(splitTime / 60);
      const paceSec = Math.floor(splitTime % 60);
      const pace = `${paceMin}:${String(paceSec).padStart(2,'0')}`;
      setSplits(s => [...s, { km: newKm, time: t, pace, splitTime }]);
      beep(880, 0.1, 0.06);
    }
    if (km >= targetKm) {
      setRunning(false);
      setDone(true);
      beep(1320, 0.2, 0.08);
      setTimeout(() => beep(1760, 0.2, 0.08), 180);
    }
  });

  const start = () => {
    if (done) {
      setDone(false); setElapsed(0); setSplits([]); baseRef.current = 0; lastSplitRef.current = 0;
    }
    startRef.current = performance.now();
    setRunning(true);
  };
  const pause = () => {
    baseRef.current += (performance.now() - startRef.current) / 1000;
    setRunning(false);
  };
  const reset = () => {
    setRunning(false); setElapsed(0); setSplits([]); setDone(false); baseRef.current = 0; lastSplitRef.current = 0;
  };

  const currentPace = elapsed > 0 && km > 0
    ? (() => { const p = elapsed / km; return `${Math.floor(p/60)}:${String(Math.floor(p%60)).padStart(2,'0')}`; })()
    : '—:—';

  // path along map
  const pathPoints = (() => {
    const total = 60;
    const used = Math.floor(progress * total);
    const pts = [];
    for (let i = 0; i <= used; i++) {
      const t = i / total;
      const x = 20 + t * 280;
      const y = 110 + Math.sin(t * Math.PI * 4) * 50 + Math.cos(t * Math.PI * 2) * 20;
      pts.push([x, y]);
    }
    return pts;
  })();
  const pathD = pathPoints.length ? 'M' + pathPoints.map(p => p.join(',')).join(' L') : '';
  const head = pathPoints[pathPoints.length - 1];

  const fb = useFeedback(done, onBack);
  const summary = `${w.target} ${w.unit} · ${fmtSec(elapsed)}${currentPace !== '\u2014:\u2014' ? ` · ${currentPace}/km` : ''}`;
  return (
    <>
    <div className="detail fade">
      <div className="detail-header">
        <button className="icon-btn" onClick={onBack}><Icon name="back" size={18}/></button>
        <div className="eyebrow">Distance · pace</div>
        <button className="icon-btn" onClick={onEdit}><Icon name="edit" size={16}/></button>
      </div>

      <div className="detail-intro">
        <div className="h-1 rise" style={{animationDelay:'40ms'}}>{w.name}</div>
        <div className="caption rise" style={{animationDelay:'100ms'}}>
          Target {w.target} {w.unit} · goal pace {w.paceTarget}/km
        </div>
      </div>

      <div className="scrollable" style={{padding:'0 22px 8px'}}>
        {/* Map */}
        <div className="map-stage rise" style={{animationDelay:'160ms'}}>
          <div className="map-grid"/>
          <svg viewBox="0 0 320 220" style={{position:'absolute', inset:0, width:'100%', height:'100%'}}>
            {/* phantom full route */}
            <path d={(() => {
              const pts = [];
              for (let i = 0; i <= 60; i++) {
                const t = i / 60;
                const x = 20 + t * 280;
                const y = 110 + Math.sin(t * Math.PI * 4) * 50 + Math.cos(t * Math.PI * 2) * 20;
                pts.push([x, y]);
              }
              return 'M' + pts.map(p => p.join(',')).join(' L');
            })()} fill="none" stroke="var(--bone-3)" strokeWidth="3" strokeDasharray="4 6"/>
            {pathD && <path d={pathD} fill="none" stroke="var(--accent)" strokeWidth="3" strokeLinecap="round"/>}
            {head && <circle cx={head[0]} cy={head[1]} r="6" fill="var(--accent)" stroke="var(--paper)" strokeWidth="3"/>}
            {head && running && <circle cx={head[0]} cy={head[1]} r="12" fill="none" stroke="var(--accent)" strokeWidth="2" opacity="0.5">
              <animate attributeName="r" values="6;18;6" dur="1.4s" repeatCount="indefinite"/>
              <animate attributeName="opacity" values="0.6;0;0.6" dur="1.4s" repeatCount="indefinite"/>
            </circle>}
          </svg>
        </div>

        {/* Live numbers */}
        <div style={{display:'grid', gridTemplateColumns:'1fr 1fr 1fr', gap:8, margin:'14px 0 16px'}}>
          {[
            ['Distance', `${km.toFixed(2)}`, 'km'],
            ['Time', fmtSec(elapsed), ''],
            ['Pace', currentPace, '/km'],
          ].map(([lbl, val, u], i) => (
            <div key={lbl} className="rise" style={{padding:'12px 10px', background:'var(--paper)', border:'1px solid var(--rule)', borderRadius:'var(--r-sm)', animationDelay:`${200 + i * 60}ms`}}>
              <div className="caption" style={{fontSize:10, letterSpacing:'0.14em', textTransform:'uppercase', marginBottom:4}}>{lbl}</div>
              <div style={{display:'flex', alignItems:'baseline', gap:4}}>
                <div className="num" style={{fontSize:18, fontWeight:600}}>{val}</div>
                <div className="caption" style={{fontSize:10}}>{u}</div>
              </div>
            </div>
          ))}
        </div>

        {/* Splits */}
        <div className="eyebrow" style={{padding:'8px 0', color:'var(--ink-3)'}}>Splits</div>
        <div className="splits">
          {Array.from({length: w.target}).map((_, i) => {
            const split = splits[i];
            const isCurrent = i === splits.length && running;
            return (
              <div key={i} className="split-row" style={{
                opacity: split || isCurrent ? 1 : 0.4,
                background: isCurrent ? 'var(--accent-soft)' : 'var(--paper)',
                borderColor: isCurrent ? 'var(--accent)' : 'var(--rule)',
                transition: 'all 0.3s'
              }}>
                <div className="km">{i + 1}</div>
                <div className="bar"><div style={{width: split ? '100%' : (isCurrent ? `${(km - i) * 100}%` : '0%')}}/></div>
                <div className="pace">{split ? split.pace : (isCurrent ? '...' : '—:—')}</div>
              </div>
            );
          })}
        </div>

        {done && (
          <div className="flash-complete" style={{margin:'18px 0 8px', padding:14, background:'var(--accent)', color:'var(--text-on-accent)', borderRadius:'var(--r-md)', display:'flex', gap:12, alignItems:'center'}}>
            <div style={{width:40, height:40, borderRadius:'50%', background:'var(--text-on-accent)', display:'flex', alignItems:'center', justifyContent:'center', color:'var(--accent)'}}>
              <Icon name="check" size={22}/>
            </div>
            <div>
              <div className="h-2" style={{color:'var(--text-on-accent)', fontSize:15}}>Run complete</div>
              <div className="caption num" style={{color:'var(--text-on-accent)', opacity:0.8}}>{w.target} km · {fmtSec(elapsed)} · avg {currentPace}/km</div>
            </div>
          </div>
        )}
      </div>

      <div className="controls">
        <button className="ctrl" onClick={reset}><Icon name="reset" size={20}/></button>
        <button className="ctrl primary" onClick={running ? pause : start}>
          <Icon name={running ? 'pause' : 'play'} size={28}/>
        </button>
        <button className="ctrl" onClick={() => { window.__audioOn = !window.__audioOn; setSplits(s => [...s]); }}>
          <Icon name="sound" size={20} style={{opacity: window.__audioOn ? 1 : 0.4}}/>
        </button>
      </div>
    </div>
    <FeedbackSheet open={fb.open} summary={summary} onSkip={fb.close} onSubmit={fb.close}/>
    </>
  );
};

const Detail = ({ w, onBack, onEdit }) => {
  if (w.type === 'time') return <TimeDetail w={w} onBack={onBack} onEdit={onEdit}/>;
  if (w.type === 'reps') return <RepsDetail w={w} onBack={onBack} onEdit={onEdit}/>;
  return <DistanceDetail w={w} onBack={onBack} onEdit={onEdit}/>;
};

window.Detail = Detail;
