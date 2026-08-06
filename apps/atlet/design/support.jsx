// support.jsx — v2 ADDITION (revenue engines #1 + #3: subscriptions + donations).
// The "fund us" hub. atlet is free; this is how the business gets paid.

const Support = ({ plan, subscriptions, donations, onSubscribe, onDonate, profile }) => {
  const [billing, setBilling] = React.useState('monthly');
  const [selectedPlan, setSelectedPlan] = React.useState(plan);
  // Tip jar — slider-first. ONE amount control (the slider is primary; the
  // chips below are accelerators, not a duplicate input). No coffee metaphor
  // on a plant-based fitness app: the amounts map to what they FUND.
  const [tipAmount, setTipAmount] = React.useState(10);
  const [tipMsg, setTipMsg] = React.useState('');
  const [tipMonthly, setTipMonthly] = React.useState(false);
  const [mode, setMode] = React.useState(plan === 'free' ? 'plans' : 'tip');
  React.useEffect(() => { setSelectedPlan(plan); }, [plan]);
  const totalDonated = donations.reduce((s, d) => s + d.amount, 0);
  const appSub = subscriptions.find(s => s.tier === 'app' && s.status === 'active');
  // What a tip funds — training metaphors, not coffee. The band drives the
  // headline copy so the amount always reads as impact, never an abstract $.
  const TIP_BANDS = [
    { max: 4,  label: 'Fuel a cooldown',     sub: 'Keeps a workout free for someone new' },
    { max: 9,  label: 'Back a training week', sub: 'Server time + audio cues for 7 days' },
    { max: 19, label: 'Sponsor a season',     sub: 'New workout types + features shipping' },
    { max: Infinity, label: 'Patron',          sub: 'You make the roadmap possible' },
  ];
  const band = TIP_BANDS.find(b => tipAmount <= b.max) || TIP_BANDS[TIP_BANDS.length - 1];
  const TIP_STOPS = [3, 5, 10, 20];

  return (
    <div className="page">
      <div className="scrollable">
        <div className="support-hero rise">
          <div className="support-hero-glyph"><AtletMark size={56}/></div>
          <div className="h-display" style={{fontSize: 28}}>Keep Atlet running.</div>
          <p className="support-hero-sub">Atlet is free and ad-free. If it helps you train, here's how to give back.</p>
          <div className="support-stats">
            <div className="support-stat"><div className="num big">{money(totalDonated)}</div><div className="lbl">you've tipped</div></div>
            <div className="support-divider"/>
            <div className="support-stat"><div className="num big">{profile.plan === 'free' ? 'Free' : profile.plan === 'pro' ? 'Pro' : 'Supporter'}</div><div className="lbl">your plan</div></div>
          </div>
        </div>
        <div className="support-tabs">
          <button className={`plan-tab ${mode === 'plans' ? 'active' : ''}`} onClick={() => setMode('plans')}><Icon name="sync" size={14}/> Membership</button>
          <button className={`plan-tab ${mode === 'tip' ? 'active' : ''}`} onClick={() => setMode('tip')}><Icon name="heart" size={14}/> Tip jar</button>
        </div>
        {mode === 'plans' && (
          <div className="support-section rise">
            <div className="plan-billing">
              <div className="mini-seg wide">
                <button className={`mini-seg-opt ${billing === 'monthly' ? 'active' : ''}`} onClick={() => setBilling('monthly')}>Monthly</button>
                <button className={`mini-seg-opt ${billing === 'yearly' ? 'active' : ''}`} onClick={() => setBilling('yearly')}>Yearly <span className="save-badge">−33%</span></button>
              </div>
            </div>
            <div className="plan-cards">
              {APP_PLANS.map(p => {
                const isCurrent = (appSub && appSub.planId === p.id) || (!appSub && p.id === 'free');
                const price = billing === 'monthly' ? p.priceM : (p.priceY / 12);
                return (
                  <button key={p.id} className={`plan-card ${selectedPlan === p.id ? 'current' : ''} ${p.featured ? 'featured' : ''}`} onClick={() => setSelectedPlan(p.id)}>
                    <div className="plan-card-head">
                      <div className="plan-card-name"><span className="plan-card-title">{p.name}</span>{p.tag && <span className={`plan-tag ${p.featured ? 'featured' : ''}`}>{p.tag}</span>}</div>
                      <div className="plan-card-price">{p.priceM === 0 ? <span className="price-big">Free</span> : <><span className="price-big num">{money(price)}</span><span className="price-per">/mo</span></>}</div>
                    </div>
                    <ul className="plan-features">{p.features.map((f, i) => <li key={i}><Icon name="check" size={13}/><span>{f}</span></li>)}</ul>
                    {isCurrent ? <div className="plan-current-tag">Current plan</div> : <div className="plan-select-tag">Select {p.name}</div>}
                  </button>
                );
              })}
            </div>
            <button className="btn" disabled={selectedPlan === plan} onClick={() => onSubscribe(selectedPlan)}>
              {selectedPlan === plan ? 'Current plan' : `Continue with ${APP_PLANS.find(p => p.id === selectedPlan).name}`}
            </button>
            <div className="plan-legal">Cancel anytime. Renews automatically.</div>
          </div>
        )}
        {mode === 'tip' && (
          <div className="support-section rise">
            <div className="tip-card">
              {/* ONE focal amount — big, live, tied to impact (not a coffee) */}
              <div className="tip-hero">
                <div className="tip-hero-amount num">{money(tipAmount)}</div>
                <div className="tip-hero-band">{band.label}</div>
                <div className="tip-hero-sub">{band.sub}</div>
              </div>

              {/* primary control: a slider. ONE way to set the amount. */}
              <div className="tip-slider-row">
                <input className="tip-range" type="range" min="1" max="50" step="1"
                  value={tipAmount} onChange={(e) => setTipAmount(Number(e.target.value))}
                  aria-label="Tip amount"/>
                <div className="tip-slider-track">
                  <span>$1</span><span>$50</span>
                </div>
              </div>

              {/* accelerators — jump to sensible stops. NOT a second input. */}
              <div className="tip-stops">
                {TIP_STOPS.map(a => (
                  <button key={a} className={`tip-stop ${tipAmount === a ? 'active' : ''}`}
                    onClick={() => setTipAmount(a)}>
                    <span className="num">{money(a)}</span>
                  </button>
                ))}
              </div>

              {/* make it recurring — recurring support is the real goal */}
              <button className={`tip-recurring ${tipMonthly ? 'on' : ''}`} onClick={() => setTipMonthly(m => !m)}>
                <div className="tip-recurring-text">
                  <div className="tip-recurring-title">Make it monthly</div>
                  <div className="tip-recurring-sub">{tipMonthly ? `${money(tipAmount)}/mo · cancel anytime` : 'One-time gift today'}</div>
                </div>
                <div className={`ios-switch ${tipMonthly ? 'on' : ''}`}/>
              </button>

              <button className="btn tip-cta" onClick={() => onDonate(tipAmount)}>
                {tipMonthly ? <><Icon name="sync" size={16}/> Give {money(tipAmount)}/mo</> : <><Icon name="heart" size={16}/> Support with {money(tipAmount)}</>}
              </button>

              {/* message belongs in the flow, below the commitment */}
              <input className="input tip-note" placeholder="Add a note to the team (optional)…"
                value={tipMsg} onChange={(e) => setTipMsg(e.target.value)} maxLength={140}/>

              <div className="plan-legal">Secure via Stripe · {tipMonthly ? 'Renews monthly, cancel anytime' : 'Charged once today'}</div>
            </div>
          </div>
        )}
        {donations.length > 0 && (
          <div className="support-section">
            <div className="list-header">Your tips</div>
            <div className="list-group">
              {donations.map(d => (
                <div key={d.id} className="list-row">
                  <div className="list-row-icon"><Icon name="heart" size={17}/></div>
                  <div className="list-row-body"><div className="list-row-title num">{money(d.amount)}</div>{d.message && <div className="list-row-detail">"{d.message}"</div>}{!d.message && <div className="list-row-detail">{d.date}</div>}</div>
                </div>
              ))}
            </div>
          </div>
        )}
        <div style={{height: 24}}/>
      </div>
    </div>
  );
};
window.Support = Support;
