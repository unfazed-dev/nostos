// sheet.jsx — Progressive bottom-sheet create/edit flow

const TYPES = [
  { id: 'time', label: 'Time', icon: 'clock', sub: 'Countdown' },
  { id: 'reps', label: 'Reps', icon: 'reps', sub: 'Tap to log' },
  { id: 'distance', label: 'Distance', icon: 'distance', sub: 'Pace tracker' },
];

const NumPad = ({ value, onChange, suffix }) => (
  <div style={{display:'flex', alignItems:'baseline', gap:8, padding:'18px 0'}}>
    <input
      className="input num"
      type="number"
      value={value}
      onChange={(e) => onChange(parseInt(e.target.value || '0', 10))}
      style={{fontSize:34, padding:'10px 14px', textAlign:'left', flex:1}}
    />
    <div className="caption" style={{fontSize:13, color:'var(--ink-3)'}}>{suffix}</div>
  </div>
);

const CreateSheet = ({ open, onClose, onSave, editing }) => {
  // expansion: 0 = type pick, 1 = name + structure, 2 = target + details
  const [stage, setStage] = React.useState(0);
  const [type, setType] = React.useState('time');
  const [name, setName] = React.useState('');
  const [structure, setStructure] = React.useState('single');
  const [target, setTarget] = React.useState(15);
  const [audio, setAudio] = React.useState(true);
  const [pulse, setPulse] = React.useState(true);
  const [tempo, setTempo] = React.useState(1); // seconds per rep
  const [restDur, setRestDur] = React.useState(45); // seconds between steps

  React.useEffect(() => {
    if (open) {
      if (editing) {
        setStage(2);
        setType(editing.type);
        setName(editing.name);
        setStructure(editing.structure);
        const t = editing.type === 'time' ? Math.round(editing.target / 60) : editing.target;
        setTarget(t);
        setTempo(editing.tempo || 1);
        setRestDur(editing.restDur ?? 45);
      } else {
        setStage(0);
        setType('time');
        setName('');
        setStructure('single');
        setTarget(15);
        setTempo(1);
        setRestDur(45);
      }
    }
  }, [open, editing]);

  // dynamic height per stage
  const heights = ['46%', '68%', '92%'];
  const heightStyle = { height: heights[stage] };

  const placeholder = type === 'time' ? 'e.g. Tabata Burnout' : type === 'reps' ? 'e.g. Push Day' : 'e.g. Sunrise 5k';
  const targetUnit = type === 'time' ? 'minutes' : type === 'reps' ? 'reps' : 'km';
  const defaultTarget = type === 'time' ? 15 : type === 'reps' ? 50 : 5;

  React.useEffect(() => {
    if (!editing) setTarget(defaultTarget);
  }, [type, editing]);

  const canAdvance = stage === 0 ? !!type : stage === 1 ? name.length > 0 : true;

  const advance = () => {
    if (stage < 2) setStage(stage + 1);
    else save();
  };

  const save = () => {
    const w = {
      id: editing?.id || `w${Date.now()}`,
      name: name || 'Untitled workout',
      type,
      target: type === 'time' ? target * 60 : target,
      unit: type === 'time' ? 'sec' : type === 'reps' ? 'reps' : 'km',
      structure,
      notes: editing?.notes || `${TYPES.find(t=>t.id===type).sub}`,
      paceTarget: type === 'distance' ? '5:30' : undefined,
      tempo: type === 'reps' ? tempo : undefined,
      restDur: structure === 'multi' ? restDur : undefined,
      steps: editing?.steps,
      last: editing?.last || 'never',
      streak: editing?.streak || 0,
      color: '#D2522B',
    };
    onSave(w);
  };

  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet ${open ? 'open' : ''}`} style={heightStyle}>
        <div className="grabber"/>
        <div className="sheet-head">
          <div style={{display:'flex', flexDirection:'column', gap:2}}>
            <div className="eyebrow">{editing ? 'Edit workout' : 'New workout'}</div>
            <div className="h-2">{stage === 0 ? 'Pick a type' : stage === 1 ? 'Name it' : 'Set the target'}</div>
          </div>
          <button className="icon-btn" onClick={onClose}><Icon name="close" size={16}/></button>
        </div>

        <div className="sheet-body">
          {/* Stage 0: type */}
          <div className="field rise" style={{animationDelay:'40ms'}}>
            <label>Workout type</label>
            <div className="type-grid">
              {TYPES.map((t, i) => (
                <div key={t.id} className={`type-tile ${type === t.id ? 'active' : ''}`} onClick={() => setType(t.id)}>
                  <Icon name={t.icon} size={22}/>
                  <div className="label">{t.label}</div>
                  <div className="caption" style={{fontSize:10, color: type === t.id ? 'var(--ink-4)' : 'var(--ink-3)'}}>{t.sub}</div>
                </div>
              ))}
            </div>
          </div>

          {/* Stage 1+: name & structure */}
          {stage >= 1 && (
            <>
              <div className="field rise" style={{animationDelay:'60ms'}}>
                <label>Name</label>
                <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder={placeholder} autoFocus={stage === 1}/>
              </div>
              <div className="field rise" style={{animationDelay:'120ms'}}>
                <label>Structure</label>
                <div className="chip-row">
                  <div className={`chip ${structure === 'single' ? 'active' : ''}`} onClick={() => setStructure('single')}>Single block</div>
                  <div className={`chip ${structure === 'multi' ? 'active' : ''}`} onClick={() => setStructure('multi')}>Multi-step</div>
                </div>
              </div>
            </>
          )}

          {/* Stage 2: target + details */}
          {stage >= 2 && (
            <>
              <div className="field rise" style={{animationDelay:'40ms'}}>
                <label>Target {targetUnit}</label>
                <NumPad value={target} onChange={setTarget} suffix={targetUnit}/>
                {/* quick chips */}
                <div className="chip-row">
                  {(type === 'time' ? [5,10,15,20,30,45] : type === 'reps' ? [20,30,50,75,100] : [3,5,7.5,10,15,21]).map(v => (
                    <div key={v} className={`chip ${target === v ? 'active' : ''}`} onClick={() => setTarget(v)}>
                      {v}
                    </div>
                  ))}
                </div>
              </div>

              {type === 'distance' && (
                <div className="field rise" style={{animationDelay:'100ms'}}>
                  <label>Goal pace</label>
                  <input className="input num" defaultValue="5:30" style={{maxWidth:140, fontSize:18}}/>
                </div>
              )}

              {type === 'reps' && (
                <div className="field rise" style={{animationDelay:'100ms'}}>
                  <label>Tempo <span style={{color:'var(--ink-3)', fontWeight:400, textTransform:'none', letterSpacing:0}}>· seconds per rep</span></label>
                  <div className="chip-row">
                    {[1, 2, 3, 4, 5, 6].map(v => (
                      <div key={v} className={`chip ${tempo === v ? 'active' : ''}`} onClick={() => setTempo(v)}>
                        {v}s
                      </div>
                    ))}
                  </div>
                  <div className="caption" style={{marginTop:8, fontSize:11, color:'var(--ink-3)'}}>
                    Auto-paced: {tempo}s per rep · {Math.round(60/tempo)} reps/min · ~{Math.ceil(target * tempo / 60 * 10)/10} min total
                  </div>
                </div>
              )}

              <div className="field rise" style={{animationDelay:'160ms'}}>
                <label>Cues during workout</label>
                <div className="toggle-row">
                  <div style={{display:'flex', flexDirection:'column', gap:2}}>
                    <div style={{fontWeight:600, fontSize:14}}>Audio cues</div>
                    <div className="caption">Beep at intervals & on completion</div>
                  </div>
                  <div className={`switch ${audio ? 'on' : ''}`} onClick={() => { setAudio(a=>!a); window.__audioOn = !audio; }}/>
                </div>
                <div className="toggle-row">
                  <div style={{display:'flex', flexDirection:'column', gap:2}}>
                    <div style={{fontWeight:600, fontSize:14}}>Visual pulse</div>
                    <div className="caption">Halfway flash & final 10-second emphasis</div>
                  </div>
                  <div className={`switch ${pulse ? 'on' : ''}`} onClick={() => setPulse(p=>!p)}/>
                </div>
              </div>

              {structure === 'multi' && (
                <div className="field rise" style={{animationDelay:'200ms'}}>
                  <label>Rest between steps</label>
                  <div className="chip-row">
                    <div className={`chip ${restDur === 0 ? 'active' : ''}`} onClick={() => setRestDur(0)}>None</div>
                    {[15, 30, 45, 60, 90, 120].map(v => (
                      <div key={v} className={`chip ${restDur === v ? 'active' : ''}`} onClick={() => setRestDur(v)}>
                        {v}s
                      </div>
                    ))}
                  </div>
                  <div className="caption" style={{marginTop:8, fontSize:11, color:'var(--ink-3)'}}>
                    {restDur === 0 ? 'No rest \u2014 steps flow straight into each other.' : `${restDur}s rest between each step \u00b7 live-adjustable during workout`}
                  </div>
                </div>
              )}

              {structure === 'multi' && (
                <div className="field rise" style={{animationDelay:'220ms'}}>
                  <label>Steps</label>
                  <div className="step-list">
                    {(editing?.steps || (type === 'time'
                      ? [{name:'Work', dur:30}, {name:'Rest', dur:15}]
                      : type === 'reps'
                      ? [{name:'Set 1', reps:Math.round(target/3)}, {name:'Set 2', reps:Math.round(target/3)}, {name:'Set 3', reps:Math.round(target/3)}]
                      : [{name:'Warmup', km:1}, {name:'Main', km:Math.max(1, target-2)}, {name:'Cool', km:1}]
                    )).map((s, i) => (
                      <div key={i} className="step-card">
                        <div className="idx">{i+1}</div>
                        <div className="body">
                          <div className="name">{s.name}</div>
                          <div className="meta">{s.dur ? `${s.dur}s` : s.reps ? `${s.reps} reps` : `${s.km}km`}</div>
                        </div>
                        <button className="icon-btn" style={{width:30, height:30}}><Icon name="more" size={14}/></button>
                      </div>
                    ))}
                    <button className="add-card" style={{margin:0, padding:12, justifyContent:'center'}}>
                      <span className="plus" style={{width:24, height:24, fontSize:14}}><Icon name="plus" size={12}/></span>
                      <span style={{fontSize:13}}>Add step</span>
                    </button>
                  </div>
                </div>
              )}
            </>
          )}
        </div>

        <div className="sheet-foot">
          <div style={{display:'flex', gap:10}}>
            {stage > 0 && (
              <button className="btn ghost" style={{width:'auto', padding:'16px 18px'}} onClick={() => setStage(stage - 1)}>
                <Icon name="back" size={16}/>
              </button>
            )}
            <button className="btn accent" onClick={advance} disabled={!canAdvance}>
              {stage < 2 ? 'Continue' : (editing ? 'Save changes' : 'Create workout')}
            </button>
          </div>
        </div>
      </div>
    </>
  );
};

window.CreateSheet = CreateSheet;
