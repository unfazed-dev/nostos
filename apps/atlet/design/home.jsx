// home.jsx — Dashboard with horizontally-scrollable stat cards + workout list

const StatBars = ({ data = WEEK_VOLUME, labels = WEEK_LABELS, max = 60 }) => (
  <div style={{display:'flex', flexDirection:'column', gap:6}}>
    <div className="bar-row">
      {data.map((v, i) => (
        <div key={i} style={{flex:1, display:'flex', flexDirection:'column', justifyContent:'flex-end', height:'100%'}}>
          <div className="bar" style={{height: `${(v/max)*100}%`, background: v>0 ? 'var(--accent)' : 'var(--bone-3)', opacity: v>0 ? 1 : 0.4}}/>
        </div>
      ))}
    </div>
    <div style={{display:'flex', gap:6, fontSize:10, color:'var(--ink-3)', fontFamily:'var(--mono)', textAlign:'center'}}>
      {labels.map((l,i)=>(<div key={i} style={{flex:1}}>{l}</div>))}
    </div>
  </div>
);

const Heatmap = ({ data = HEAT_DATA }) => (
  <div style={{display:'grid', gridTemplateColumns:'repeat(8, 1fr)', gap:4}}>
    {data.map((week, wi) =>
      week.map((v, di) => {
        // Activity heat: empty cells = faint bone, active cells = accent at
        // rising intensity. Was rgba(245,240,232,…) (light --bone literal) —
        // invisible on the featured card AND in light theme. (GLM-5.2)
        const pct = v === 0 ? 8 : v === 1 ? 35 : v === 2 ? 65 : 100;
        return <div key={`${wi}-${di}`} style={{aspectRatio:'1', borderRadius:3, background:`color-mix(in srgb, var(--accent) ${pct}%, rgba(245,240,232,0.12))`}}/>;
      })
    )}
  </div>
);

const TrendLine = ({ points = TREND_POINTS }) => {
  const w = 280, h = 70, pad = 4;
  const max = Math.max(...points), min = Math.min(...points);
  const range = max - min || 1;
  const xs = points.map((_, i) => pad + (i / (points.length - 1)) * (w - pad*2));
  const ys = points.map(p => pad + (1 - (p - min) / range) * (h - pad*2));
  const d = xs.map((x, i) => `${i===0?'M':'L'}${x},${ys[i]}`).join(' ');
  const area = `${d} L${xs[xs.length-1]},${h} L${xs[0]},${h} Z`;
  return (
    <svg className="trend-svg" viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none">
      <defs>
        <linearGradient id="trendgrad" x1="0" x2="0" y1="0" y2="1">
          <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.4"/>
          <stop offset="100%" stopColor="var(--accent)" stopOpacity="0"/>
        </linearGradient>
      </defs>
      <path d={area} fill="url(#trendgrad)"/>
      <path d={d} fill="none" stroke="var(--accent)" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"/>
      {xs.map((x,i)=>(
        <circle key={i} cx={x} cy={ys[i]} r={i===xs.length-1?3.5:0} fill="var(--accent)"/>
      ))}
    </svg>
  );
};

const StatCard = ({ children, variant = 'paper', delay = 0 }) => (
  <div className={`stat-card ${variant === 'featured' ? 'featured' : variant === 'accent' ? 'accent' : ''} rise`} style={{animationDelay: `${delay}ms`}}>
    {children}
  </div>
);

const StatsDeck = ({ activeIdx, onScroll }) => {
  const ref = React.useRef(null);
  React.useEffect(() => {
    const el = ref.current;
    if (!el) return;
    let raf = null;
    const onS = () => {
      if (raf) cancelAnimationFrame(raf);
      raf = requestAnimationFrame(() => {
        const cards = el.querySelectorAll('.stat-card');
        let best = 0, bestDist = Infinity;
        const center = el.scrollLeft + el.clientWidth / 2;
        cards.forEach((c, i) => {
          const cardCenter = c.offsetLeft + c.offsetWidth / 2;
          const d = Math.abs(cardCenter - center);
          if (d < bestDist) { bestDist = d; best = i; }
        });
        onScroll(best);
      });
    };
    el.addEventListener('scroll', onS, { passive: true });

    // Drag-to-scroll for desktop (mouse)
    let isDown = false, startX = 0, startScroll = 0, moved = 0;
    const down = (e) => {
      isDown = true; moved = 0;
      startX = e.pageX;
      startScroll = el.scrollLeft;
      el.classList.add('dragging');
    };
    const move = (e) => {
      if (!isDown) return;
      const dx = e.pageX - startX;
      moved = Math.abs(dx);
      el.scrollLeft = startScroll - dx;
    };
    const up = () => {
      if (!isDown) return;
      isDown = false;
      el.classList.remove('dragging');
      // snap to nearest
      const cards = el.querySelectorAll('.stat-card');
      const center = el.scrollLeft + el.clientWidth / 2;
      let best = cards[0], bestDist = Infinity;
      cards.forEach(c => {
        const cc = c.offsetLeft + c.offsetWidth / 2;
        const d = Math.abs(cc - center);
        if (d < bestDist) { bestDist = d; best = c; }
      });
      el.scrollTo({ left: best.offsetLeft - (el.clientWidth - best.offsetWidth) / 2, behavior: 'smooth' });
    };
    // Wheel: convert vertical wheel to horizontal scroll on the deck
    const wheel = (e) => {
      if (Math.abs(e.deltaY) > Math.abs(e.deltaX)) {
        el.scrollLeft += e.deltaY;
        e.preventDefault();
      }
    };
    el.addEventListener('mousedown', down);
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', up);
    el.addEventListener('wheel', wheel, { passive: false });

    return () => {
      el.removeEventListener('scroll', onS);
      el.removeEventListener('mousedown', down);
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', up);
      el.removeEventListener('wheel', wheel);
    };
  }, [onScroll]);

  return (
    <div className="stats-deck" ref={ref}>
      {/* Card 1: Weekly volume */}
      <StatCard variant="paper" delay={80}>
        <div className="stat-head">
          <div className="eyebrow">This week</div>
          <div className="caption num">+18%</div>
        </div>
        <div className="stat-headline">
          <div className="big">2:31</div>
          <div className="unit">hours moved</div>
        </div>
        <StatBars />
      </StatCard>

      {/* Card 2: Streak heatmap — featured brand surface (always warm-dark) */}
      <StatCard variant="featured" delay={140}>
        <div className="stat-head">
          <div className="eyebrow">Consistency</div>
          <div className="caption num">8 weeks</div>
        </div>
        <div className="stat-headline">
          <div className="big">14</div>
          <div className="unit">day streak</div>
        </div>
        <Heatmap />
      </StatCard>

      {/* Card 3: Trend (accent) */}
      <StatCard variant="paper" delay={200}>
        <div className="stat-head">
          <div className="eyebrow">Trend · 7 weeks</div>
          <div className="caption num" style={{color:'var(--accent-2)'}}>↑ trending</div>
        </div>
        <div className="stat-headline">
          <div className="big">155</div>
          <div className="unit">min last week</div>
        </div>
        <TrendLine />
      </StatCard>

      {/* Card 4: Today */}
      <StatCard variant="accent" delay={260}>
        <div className="stat-head">
          <div className="eyebrow">Today</div>
          <div className="caption">Wed, Apr 18</div>
        </div>
        <div style={{display:'flex', flexDirection:'column', gap:6, marginTop:'auto'}}>
          <div className="stat-headline">
            <div className="big" style={{fontSize:32}}>2</div>
            <div className="unit" style={{fontSize:12}}>workouts queued</div>
          </div>
          <div className="caption" style={{fontSize:12, fontWeight:500, marginTop:6, lineHeight:1.4, opacity:0.85}}>
            Sunrise 5k · Tabata Burnout
          </div>
        </div>
      </StatCard>
    </div>
  );
};

const WorkoutCard = ({ w, onOpen, onPlay, onDelete, onDuplicate, delay = 0 }) => {
  const meta = TYPE_META[w.type];
  let bigVal, bigUnit;
  if (w.type === 'time') {
    bigVal = w.target >= 60 ? `${Math.floor(w.target/60)}` : `${w.target}`;
    bigUnit = w.target >= 60 ? 'minutes' : 'seconds';
  } else if (w.type === 'reps') {
    bigVal = String(w.target);
    bigUnit = 'reps total';
  } else {
    bigVal = String(w.target);
    bigUnit = w.unit;
  }

  // Swipe + long-press state
  const [dragX, setDragX] = React.useState(0);
  const [dragging, setDragging] = React.useState(false);
  const [committed, setCommitted] = React.useState(false); // after release: true = stay open, false = snap back
  const [exiting, setExiting] = React.useState(false);
  const [menuOpen, setMenuOpen] = React.useState(false);
  const startX = React.useRef(0);
  const startY = React.useRef(0);
  const moved = React.useRef(false);
  const longPressTimer = React.useRef(null);
  const pressDidTrigger = React.useRef(false);

  const THRESHOLD_REVEAL = 64;
  const THRESHOLD_DELETE = 180;

  const clearLongPress = () => {
    if (longPressTimer.current) {
      clearTimeout(longPressTimer.current);
      longPressTimer.current = null;
    }
  };

  const haptic = (intensity = 'light') => {
    if (navigator.vibrate) navigator.vibrate(intensity === 'heavy' ? 20 : intensity === 'medium' ? 12 : 6);
  };

  const onPointerDown = (e) => {
    if (menuOpen) return;
    startX.current = e.clientX;
    startY.current = e.clientY;
    moved.current = false;
    pressDidTrigger.current = false;
    setDragging(true);
    // Long-press for context menu
    longPressTimer.current = setTimeout(() => {
      if (!moved.current) {
        pressDidTrigger.current = true;
        haptic('heavy');
        setMenuOpen(true);
        setDragging(false);
        setDragX(0);
      }
    }, 500);
  };

  const onPointerMove = (e) => {
    if (!dragging) return;
    const dx = e.clientX - startX.current;
    const dy = e.clientY - startY.current;
    if (!moved.current && (Math.abs(dx) > 6 || Math.abs(dy) > 6)) {
      moved.current = true;
      clearLongPress();
    }
    if (Math.abs(dy) > Math.abs(dx)) {
      // vertical scroll — abandon drag
      setDragging(false);
      setDragX(committed ? -THRESHOLD_REVEAL : 0);
      return;
    }
    // Left-swipe only; allow small elastic right
    const start = committed ? -THRESHOLD_REVEAL : 0;
    let next = start + dx;
    if (next > 0) next = next * 0.25;
    if (next < -THRESHOLD_DELETE - 40) next = -THRESHOLD_DELETE - 40 + (next + THRESHOLD_DELETE + 40) * 0.25;
    setDragX(next);
  };

  const onPointerUp = (e) => {
    clearLongPress();
    if (!dragging) return;
    setDragging(false);
    if (!moved.current) {
      // Tap: open detail (unless long-press triggered)
      if (!pressDidTrigger.current) onOpen(w);
      setDragX(committed ? -THRESHOLD_REVEAL : 0);
      return;
    }
    if (dragX <= -THRESHOLD_DELETE) {
      haptic('medium');
      setExiting(true);
      setTimeout(() => onDelete && onDelete(w), 260);
    } else if (dragX <= -THRESHOLD_REVEAL * 0.6) {
      setCommitted(true);
      setDragX(-THRESHOLD_REVEAL);
      haptic('light');
    } else {
      setCommitted(false);
      setDragX(0);
    }
  };

  const onPointerCancel = () => {
    clearLongPress();
    setDragging(false);
    setDragX(committed ? -THRESHOLD_REVEAL : 0);
  };

  const closeMenu = () => setMenuOpen(false);

  const handleDelete = () => {
    haptic('medium');
    setExiting(true);
    setTimeout(() => onDelete && onDelete(w), 260);
  };

  return (
    <>
      <div
        className={`workout-swipe ${exiting ? 'exiting' : ''}`}
        style={{animationDelay: `${delay}ms`}}
      >
        <div className="workout-swipe-action">
          <button className="swipe-delete" onClick={handleDelete}>
            <Icon name="trash" size={18}/>
            <span>Delete</span>
          </button>
        </div>
        <div
          className={`workout-card ${dragging || committed || menuOpen ? 'no-anim' : ''}`}
          style={{transform: `translateX(${dragX}px)`, transition: dragging ? 'none' : 'transform 0.32s cubic-bezier(.25,.9,.3,1)'}}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          onContextMenu={(e) => { e.preventDefault(); haptic('heavy'); setMenuOpen(true); }}
        >
          <div className="row">
            <div style={{display:'flex', flexDirection:'column', gap:6}}>
              <div className="type-pip">
                <span className="swatch"/>
                {meta.label}
              </div>
              <div className="h-2" style={{marginTop:2}}>{w.name}</div>
            </div>
            <div className="target">
              <div className="big">{bigVal}</div>
              <div className="unit">{bigUnit}</div>
            </div>
          </div>
          <div className="footer">
            <div style={{display:'flex', flexDirection:'column', gap:2}}>
              <div className="caption">
                {w.structure === 'multi' ? `${w.steps.length} steps · ${w.notes}` : w.notes}
              </div>
              <div className="caption num" style={{color:'var(--ink-4)'}}>
                Last {w.last} · {w.streak}× streak
              </div>
            </div>
            <button className="play" onClick={(e) => { e.stopPropagation(); onPlay(w); }}>
              <Icon name="play" size={14}/>
            </button>
          </div>
        </div>
      </div>
      {menuOpen && (
        <div className="ctx-overlay" onClick={closeMenu}>
          <div className="ctx-card-preview">
            <div className="workout-card" onClick={e => e.stopPropagation()}>
              <div className="row">
                <div style={{display:'flex', flexDirection:'column', gap:6}}>
                  <div className="type-pip"><span className="swatch"/>{meta.label}</div>
                  <div className="h-2" style={{marginTop:2}}>{w.name}</div>
                </div>
                <div className="target">
                  <div className="big">{bigVal}</div>
                  <div className="unit">{bigUnit}</div>
                </div>
              </div>
            </div>
          </div>
          <div className="ctx-menu" onClick={e => e.stopPropagation()}>
            <button className="ctx-item" onClick={() => { closeMenu(); onOpen(w); }}>
              <span>Open</span><Icon name="chevron" size={14}/>
            </button>
            <button className="ctx-item" onClick={() => { closeMenu(); onPlay(w); }}>
              <span>Start workout</span><Icon name="play" size={14}/>
            </button>
            <button className="ctx-item" onClick={() => { closeMenu(); onDuplicate && onDuplicate(w); }}>
              <span>Duplicate</span><Icon name="plus" size={14}/>
            </button>
            <div className="ctx-sep"/>
            <button className="ctx-item danger" onClick={() => { closeMenu(); handleDelete(); }}>
              <span>Delete</span><Icon name="trash" size={14}/>
            </button>
          </div>
        </div>
      )}
    </>
  );
};

const Home = ({ workouts, onOpen, onPlay, onCreate, onDelete, onDuplicate, onRefresh, onAccount }) => {
  const [activeStat, setActiveStat] = React.useState(0);
  const [filter, setFilter] = React.useState('all');
  const [pullY, setPullY] = React.useState(0);
  const [refreshing, setRefreshing] = React.useState(false);
  const pullStart = React.useRef(null);
  const scrollRef = React.useRef(null);
  const PULL_THRESHOLD = 72;

  const onPullStart = (e) => {
    if (scrollRef.current && scrollRef.current.scrollTop <= 0 && !refreshing) {
      pullStart.current = e.clientY;
    }
  };
  const onPullMove = (e) => {
    if (pullStart.current == null) return;
    const dy = e.clientY - pullStart.current;
    if (dy <= 0) { setPullY(0); return; }
    // resistance
    const resisted = dy < 120 ? dy * 0.55 : 66 + (dy - 120) * 0.25;
    setPullY(Math.min(resisted, 110));
  };
  const onPullEnd = () => {
    if (pullStart.current == null) return;
    pullStart.current = null;
    if (pullY >= PULL_THRESHOLD) {
      setRefreshing(true);
      setPullY(60);
      if (navigator.vibrate) navigator.vibrate(10);
      Promise.resolve(onRefresh && onRefresh()).then(() => {
        setTimeout(() => {
          setRefreshing(false);
          setPullY(0);
        }, 900);
      });
    } else {
      setPullY(0);
    }
  };

  const counts = React.useMemo(() => ({
    all: workouts.length,
    reps: workouts.filter(w => w.type === 'reps').length,
    distance: workouts.filter(w => w.type === 'distance').length,
    time: workouts.filter(w => w.type === 'time').length,
  }), [workouts]);

  const filtered = filter === 'all' ? workouts : workouts.filter(w => w.type === filter);

  const FILTERS = [
    { id: 'all', label: 'All', icon: null },
    { id: 'reps', label: 'Reps', icon: 'reps' },
    { id: 'distance', label: 'Distance', icon: 'distance' },
    { id: 'time', label: 'Time', icon: 'clock' },
  ];

  return (
    <div className="page">
      <div
        className="scrollable"
        ref={scrollRef}
        onPointerDown={onPullStart}
        onPointerMove={onPullMove}
        onPointerUp={onPullEnd}
        onPointerCancel={onPullEnd}
      >
        <div
          className="ptr-wrap"
          style={{
            transform: `translateY(${pullY}px)`,
            transition: pullStart.current ? 'none' : 'transform 0.32s cubic-bezier(.25,.9,.3,1)'
          }}
        >
          <div className="ptr-spinner" style={{opacity: Math.min(pullY / 50, 1)}}>
            <svg width="22" height="22" viewBox="0 0 22 22" style={{
              transform: refreshing
                ? 'rotate(0deg)'
                : `rotate(${(pullY / PULL_THRESHOLD) * 360}deg)`,
              animation: refreshing ? 'ptr-spin 0.9s linear infinite' : 'none'
            }}>
              <circle cx="11" cy="11" r="8" fill="none" stroke="var(--ink-3)" strokeOpacity="0.25" strokeWidth="2"/>
              <path d="M 11 3 A 8 8 0 0 1 19 11" fill="none" stroke="var(--ink-2)" strokeWidth="2" strokeLinecap="round"/>
            </svg>
          </div>
        <div className="app-header">
          <div className="greeting">
            <div className="eyebrow rise" style={{animationDelay:'0ms'}}>Wednesday · 06:42</div>
            <div className="h-display rise" style={{animationDelay:'40ms'}}>
              Morning,<br/>Alex.
            </div>
          </div>
          <button className="avatar rise" style={{animationDelay:'120ms'}} onClick={onAccount} aria-label="Account">AK</button>
        </div>

        <StatsDeck activeIdx={activeStat} onScroll={setActiveStat}/>

        <div className="dots rise" style={{animationDelay:'320ms'}}>
          {[0,1,2,3].map(i => (
            <div key={i} className={`dot ${i === activeStat ? 'active' : ''}`}/>
          ))}
        </div>

        <div className="workouts-head rise" style={{animationDelay:'360ms'}}>
          <div className="workouts-head-row">
            <div className="workouts-title">
              <div className="h-2">Your workouts</div>
              <div className="workouts-count-badge">{counts[filter]}</div>
            </div>
            <button className="new-inline-btn" onClick={onCreate}>
              <Icon name="plus" size={14}/>
              <span>New</span>
            </button>
          </div>

          <div className="segmented">
            {FILTERS.map(f => (
              <button
                key={f.id}
                className={`seg-btn ${filter === f.id ? 'active' : ''}`}
                onClick={() => setFilter(f.id)}
              >
                <span className="seg-label">{f.label}</span>
                <span className="seg-count">{counts[f.id]}</span>
              </button>
            ))}
            <div
              className="seg-indicator"
              style={{
                width: `calc(${100 / FILTERS.length}% - 4px)`,
                transform: `translateX(calc(${FILTERS.findIndex(f => f.id === filter) * 100}% + 2px))`
              }}
            />
          </div>
        </div>

        <div className="workout-list">
          {filtered.length > 0 ? filtered.map((w, i) => (
            <WorkoutCard key={w.id} w={w} onOpen={onOpen} onPlay={onPlay} onDelete={onDelete} onDuplicate={onDuplicate} delay={400 + i * 70}/>
          )) : (
            <div className="empty-state">
              <div className="empty-title">No {filter} workouts yet</div>
              <div className="empty-sub">Tap New to add one.</div>
            </div>
          )}
        </div>
        </div>{/* /ptr-wrap */}
      </div>
    </div>
  );
};

window.Home = Home;
