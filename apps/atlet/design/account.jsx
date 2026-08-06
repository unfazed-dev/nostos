/* ============================================================
   Account.jsx — HIG-style settings hub with inset-grouped lists
   Sections: Profile · Plan · Preferences · Audio · Defaults ·
   Data · About.  Opens Plan sheet for subscription/donation.
   ============================================================ */

const AccountView = ({ open, onClose, tweakState, setTweakState, onSignOut, showToast,
  // v2 commerce state (additive — the shop/sub/donation lifecycle surfaces here)
  orders = [], setOrders, subscriptions = [], setSubscriptions,
  addresses = [], setAddresses, invoices = [], donations = [],
  paymentMethods = [], setPaymentMethods, profile }) => {
  const [planSheet, setPlanSheet] = React.useState(false);
  const [editProfile, setEditProfile] = React.useState(false);
  const [profileState, setProfileState] = React.useState({ name: 'Alex Kova', email: 'alex@atlet.app', avatar: 'AK' });

  // push-view nav stack — the React-prototype equivalent of UINavigationController.
  // Each entry is { view, payload }. Commerce drill-downs push onto this; back pops.
  const [stack, setStack] = React.useState([]);
  const push = (view, payload) => setStack(s => [...s, { view, payload }]);
  const pop = () => setStack(s => s.slice(0, -1));
  React.useEffect(() => { if (!open) setStack([]); }, [open]); // reset when account closes

  const haptics = tweakState.haptics !== false;
  const voice = tweakState.voice !== false;
  const beeps = tweakState.beeps !== false;
  const autoAdvance = tweakState.autoAdvance !== false;
  const units = tweakState.units || 'km';
  const theme = tweakState.theme || 'auto';

  const set = (patch) => setTweakState({...tweakState, ...patch});

  const [plan, setPlan] = React.useState(tweakState.plan || 'free');

  // active subscriptions — the top of the commerce hub (ongoing lifecycle).
  const activeSubs = subscriptions.filter(s => s.status === 'active');
  const orderCount = orders.length;
  const defaultAddr = addresses.find(a => a.default) || addresses[0];

  return (
    <>
      {/* Account is now a normal in-flow page (not a z-70 overlay). This fixes
         three bugs at once: (1) the TabBar stays visible on Account; (2) the
         EditProfileSheet (z-60) is no longer hidden behind a z-70 overlay; (3)
         there's no "Home" back button — the TabBar owns navigation. */}
      <div className="page has-tabbar">
        <div className="scrollable account-scroll">
          {/* large-title header (replaces the old .account-nav Home bar) */}
          <div className="page-large-title">Account</div>

          {/* Profile hero */}
          <div className="profile-hero">
            <div className="profile-avatar" style={profileState.avatarColor ? {background: profileState.avatarColor, color: '#fff'} : {}}>{profileState.avatar}</div>
            <div className="profile-name">{profileState.name}</div>
            <div className="profile-email">{profileState.email}</div>
            <button className="btn tinted" style={{width:'auto', padding:'8px 18px', minHeight: 34}}
              onClick={() => setEditProfile(true)}>
              Edit profile
            </button>
          </div>

          {/* Plan section */}
          <ListGroup header="Plan">
            <Row
              icon={<PlanGlyph plan={plan}/>}
              title={plan === 'free' ? 'Free' : plan === 'pro' ? 'Pro' : 'Supporter'}
              trailing={<PlanBadge plan={plan}/>}
              detail={plan === 'free' ? 'Basic tracking · Upgrade for more' : plan === 'pro' ? 'All features · Renews monthly' : 'Thank you for supporting Atlet'}
              onTap={() => setPlanSheet(true)}
              chevron
            />
            <Row
              icon={<Icon name="heart" size={17}/>}
              title="Send a tip"
              detail="Support development · From $2"
              onTap={() => setPlanSheet(true)}
              chevron
            />
          </ListGroup>

          {/* v2 commerce — Subscriptions (top: ongoing lifecycle) */}
          <ListGroup header="Subscriptions">
            {activeSubs.length === 0 && (
              <Row icon={<Icon name="sync" size={17}/>} title="No active subscriptions"
                detail="Manage your plan or product refills here"/>
            )}
            {activeSubs.map(s => (
              <Row key={s.id}
                icon={<Icon name={s.tier === 'product' ? 'box' : 'sync'} size={17}/>}
                title={s.tier === 'product' ? s.name : s.planName}
                detail={`${money(s.price)}/${s.cadence === 'yearly' ? 'yr' : 'mo'} · Renews ${s.nextDate}`}
                onTap={() => push('subscription', s)}
                chevron
              />
            ))}
          </ListGroup>

          {/* v2 commerce — Orders & History (passive but recent) */}
          <ListGroup header="Orders & History">
            {orderCount === 0 ? (
              <Row icon={<Icon name="truck" size={17}/>} title="No orders yet"
                detail="Your shop orders will appear here"/>
            ) : (
              <Row
                icon={<Icon name="box" size={17}/>}
                title={`${orderCount} order${orderCount === 1 ? '' : 's'}`}
                detail={`Last: ${orders[0].number} · ${orders[0].date}`}
                onTap={() => push('orders')}
                chevron
              />
            )}
            <Row
              icon={<Icon name="receipt" size={17}/>}
              title="Receipts & invoices"
              detail={invoices.length ? `${invoices.length} on file` : 'None yet'}
              onTap={() => push('invoices')}
              chevron
            />
          </ListGroup>

          {/* v2 commerce — Payment & Shipping (settings) */}
          <ListGroup header="Payment & Shipping">
            <Row
              icon={<Icon name="pin" size={17}/>}
              title="Shipping addresses"
              detail={defaultAddr ? `${defaultAddr.label} · ${defaultAddr.city}` : 'Add an address'}
              onTap={() => push('addresses')}
              chevron
            />
            <Row
              icon={<Icon name="card" size={17}/>}
              title="Payment methods"
              detail={paymentMethods.length ? `${paymentMethods.length} card${paymentMethods.length === 1 ? '' : 's'} · ${paymentMethods.find(p=>p.default) ? '•• ' + paymentMethods.find(p=>p.default).last4 : paymentMethods[0].last4}` : 'Add a card'}
              onTap={() => push('payment-methods')}
              chevron
            />
          </ListGroup>

          {/* v2 commerce — Tip history (donations get their own drill-in) */}
          <ListGroup header="Tips">
            <Row
              icon={<Icon name="heart" size={17}/>}
              title="Tip history"
              detail={donations.length ? `${donations.length} tip${donations.length === 1 ? '' : 's'} · ${money(donations.reduce((s,d)=>s+d.amount,0))} given` : 'No tips yet'}
              onTap={() => push('tips')}
              chevron
            />
          </ListGroup>

          {/* Preferences */}
          <ListGroup header="Preferences">
            <RowNav
              icon={<ThemeGlyph/>}
              title="Appearance"
              trailing={<TrailingText>{theme === 'auto' ? 'Auto' : theme === 'dark' ? 'Dark' : 'Light'}</TrailingText>}
              onTap={() => set({_sub: 'theme'})}
            />
            <ThemePicker value={theme} onChange={(v) => set({theme: v})}/>
            <RowNav
              icon={<Icon name="drop" size={17}/>}
              title="Accent color"
              trailing={<AccentSwatch hue={tweakState.accentHue}/>}
            />
            <AccentPicker value={tweakState.accentHue} onChange={(h) => set({accentHue: h})}/>
            <Row
              icon={<Icon name="ruler" size={17}/>}
              title="Units"
              trailing={
                <Segmented options={[{v:'km',l:'km'},{v:'mi',l:'mi'}]} value={units} onChange={(v) => set({units: v})}/>
              }
            />
          </ListGroup>

          {/* Audio & Haptics */}
          <ListGroup header="Audio & Haptics" footer="Audio cues and vibrations during workouts.">
            <RowSwitch
              icon={<Icon name="volume" size={17}/>}
              title="Voice cues"
              detail="Countdown, reps, and rest announcements"
              value={voice}
              onChange={(v) => set({voice: v})}
            />
            <RowSwitch
              icon={<Icon name="bell" size={17}/>}
              title="Start & end beeps"
              value={beeps}
              onChange={(v) => set({beeps: v})}
            />
            <RowSwitch
              icon={<Icon name="pulse" size={17}/>}
              title="Haptic feedback"
              value={haptics}
              onChange={(v) => set({haptics: v})}
            />
          </ListGroup>

          {/* Workout defaults */}
          <ListGroup header="Workout Defaults">
            <Row
              icon={<Icon name="clock" size={17}/>}
              title="Default rest"
              trailing={<TrailingText>{tweakState.restSec || 30}s</TrailingText>}
            />
            <Row
              icon={<Icon name="flame" size={17}/>}
              title="Warm-up reminder"
              trailing={<TrailingText>{tweakState.warmup || 'On'}</TrailingText>}
            />
            <RowSwitch
              icon={<Icon name="arrow-right" size={17}/>}
              title="Auto-advance steps"
              detail="Move to next step automatically on completion"
              value={autoAdvance}
              onChange={(v) => set({autoAdvance: v})}
            />
          </ListGroup>

          {/* Data */}
          <ListGroup header="Data">
            <Row icon={<Icon name="download" size={17}/>} title="Export history"
              detail="Download as CSV" chevron onTap={() => showToast('Export started')}/>
            <Row icon={<Icon name="upload" size={17}/>} title="Import workouts"
              chevron onTap={() => showToast('Coming soon')}/>
            <Row
              icon={<Icon name="trash" size={17} color="var(--danger)"/>}
              title="Clear all history"
              destructive
              onTap={() => showToast('Hold to confirm')}
            />
          </ListGroup>

          {/* About */}
          <ListGroup header="About">
            <Row icon={<Icon name="info" size={17}/>} title="Version" trailing={<TrailingText>1.0.0 · b412</TrailingText>}/>
            <Row icon={<Icon name="doc" size={17}/>} title="Terms of Service" chevron/>
            <Row icon={<Icon name="lock" size={17}/>} title="Privacy Policy" chevron/>
            <Row icon={<Icon name="help" size={17}/>} title="Support" chevron/>
            <Row icon={<Icon name="star" size={17}/>} title="Rate Atlet" chevron/>
          </ListGroup>

          {/* Sign out */}
          <div className="account-signout">
            <button className="btn destructive-plain" onClick={onSignOut}>Sign out</button>
          </div>

          <div className="account-foot">
            Made with care. Atlet · v1.0.0
          </div>
        </div>
      </div>

      {/* v2 commerce push-view stack — overlays the Account page (translateX from
          right). Sibling of the page (not nested) so it sits above page content
          but the TabBar (z-40) stays visible on top. Top of stack = visible. */}
      {stack.length > 0 && (
        <AccountPushView
          stack={stack} onBack={pop}
          orders={orders} subscriptions={subscriptions} setSubscriptions={setSubscriptions}
          addresses={addresses} setAddresses={setAddresses} invoices={invoices} donations={donations}
          paymentMethods={paymentMethods} setPaymentMethods={setPaymentMethods}
          profile={profile} showToast={showToast}
          push={push}
        />
      )}

      <PlanSheet open={planSheet} onClose={() => setPlanSheet(false)} plan={plan} setPlan={(p) => { setPlan(p); set({plan: p}); }} showToast={showToast}/>
      <EditProfileSheet open={editProfile} onClose={() => setEditProfile(false)} profile={profileState} onSave={(p) => { setProfileState(p); setEditProfile(false); showToast('Profile saved'); }}/>
    </>
  );
};

/* === Inset grouped list (HIG) === */
const ListGroup = ({ header, footer, children }) => (
  <div className="list-group-wrap">
    {header && <div className="list-header">{header}</div>}
    <div className="list-group">{children}</div>
    {footer && <div className="list-footer">{footer}</div>}
  </div>
);

const Row = ({ icon, title, detail, trailing, onTap, chevron, destructive }) => (
  <button className={`list-row ${destructive ? 'destructive' : ''} ${onTap ? 'tappable' : ''}`} onClick={onTap} disabled={!onTap}>
    {icon && <div className="list-row-icon">{icon}</div>}
    <div className="list-row-body">
      <div className="list-row-title">{title}</div>
      {detail && <div className="list-row-detail">{detail}</div>}
    </div>
    {trailing && <div className="list-row-trail">{trailing}</div>}
    {chevron && <div className="list-row-chev"><Icon name="chevron-right" size={14}/></div>}
  </button>
);

const RowNav = Row;

const RowSwitch = ({ icon, title, detail, value, onChange }) => (
  <div className="list-row">
    {icon && <div className="list-row-icon">{icon}</div>}
    <div className="list-row-body">
      <div className="list-row-title">{title}</div>
      {detail && <div className="list-row-detail">{detail}</div>}
    </div>
    <button
      className={`ios-switch ${value ? 'on' : ''}`}
      onClick={() => onChange(!value)}
      aria-label={`Toggle ${title}`}
      aria-checked={value}
      role="switch"
    />
  </div>
);

const TrailingText = ({ children }) => <span className="trail-text">{children}</span>;

/* Theme inline picker (segmented below row) */
const ThemePicker = ({ value, onChange }) => (
  <div className="row-inline-picker">
    <div className="seg-fullwidth">
      {[
        {v:'light', l:'Light'},
        {v:'dark', l:'Dark'},
        {v:'auto', l:'Auto'},
      ].map(o => (
        <button key={o.v} className={`seg-opt ${value === o.v ? 'active' : ''}`} onClick={() => onChange(o.v)}>
          {o.l}
        </button>
      ))}
    </div>
  </div>
);

/* Accent color inline picker */
const AccentPicker = ({ value, onChange }) => {
  const accents = [
    { hue: 28,  name: 'Burnt' },
    { hue: 12,  name: 'Brick' },
    { hue: 145, name: 'Forest' },
    { hue: 220, name: 'Cobalt' },
    { hue: 280, name: 'Plum' },
    { hue: 340, name: 'Rose' },
  ];
  return (
    <div className="row-inline-picker">
      <div className="accent-row">
        {accents.map(a => (
          <button
            key={a.hue}
            className={`accent-dot ${value === a.hue ? 'active' : ''}`}
            style={{background: `oklch(60% 0.16 ${a.hue})`}}
            onClick={() => onChange(a.hue)}
            aria-label={a.name}
          />
        ))}
      </div>
    </div>
  );
};

const AccentSwatch = ({ hue }) => (
  <div className="accent-swatch" style={{background: `oklch(60% 0.16 ${hue})`}}/>
);

const Segmented = ({ options, value, onChange }) => (
  <div className="mini-seg">
    {options.map(o => (
      <button key={o.v} className={`mini-seg-opt ${value === o.v ? 'active' : ''}`} onClick={() => onChange(o.v)}>
        {o.l}
      </button>
    ))}
  </div>
);

const ThemeGlyph = () => (
  <svg width="17" height="17" viewBox="0 0 17 17" fill="none">
    <circle cx="8.5" cy="8.5" r="6.5" stroke="currentColor" strokeWidth="1.4"/>
    <path d="M 8.5 2 A 6.5 6.5 0 0 1 8.5 15" fill="currentColor"/>
  </svg>
);

const PlanGlyph = ({ plan }) => (
  <svg width="17" height="17" viewBox="0 0 17 17" fill="none">
    {plan === 'free' && <circle cx="8.5" cy="8.5" r="6" stroke="currentColor" strokeWidth="1.4"/>}
    {plan === 'pro' && <path d="M 8.5 2 L 10.5 6.6 L 15 7 L 11.5 10 L 12.5 14.5 L 8.5 12 L 4.5 14.5 L 5.5 10 L 2 7 L 6.5 6.6 Z" fill="currentColor"/>}
    {plan === 'supporter' && <path d="M 8.5 14.5 L 2.5 9 Q 1 7 2.5 5 Q 4.5 3 6.5 4.5 Q 8.5 6 8.5 6 Q 8.5 6 10.5 4.5 Q 12.5 3 14.5 5 Q 16 7 14.5 9 Z" fill="currentColor"/>}
  </svg>
);

const PlanBadge = ({ plan }) => (
  <span className={`plan-badge plan-${plan}`}>
    {plan === 'free' ? 'FREE' : plan === 'pro' ? 'PRO' : '♥'}
  </span>
);

/* ============================================================
   Plan sheet — subscription tiers + donation
   ============================================================ */
const PlanSheet = ({ open, onClose, plan, setPlan, showToast }) => {
  const [billing, setBilling] = React.useState('monthly');
  const [tipAmount, setTipAmount] = React.useState(5);
  const [mode, setMode] = React.useState('plans'); // 'plans' | 'tip'

  React.useEffect(() => { if (open) setMode('plans'); }, [open]);

  const plans = [
    {
      id: 'free', name: 'Free', tag: 'Starter',
      priceM: 0, priceY: 0,
      features: ['3 custom workouts', 'Basic tracking', '30-day history', 'Community support'],
    },
    {
      id: 'pro', name: 'Pro', tag: 'Most popular', featured: true,
      priceM: 4.99, priceY: 39.99,
      features: ['Unlimited workouts', 'Advanced charts & trends', 'Full history & export', 'Voice cues in 6 languages', 'Priority support'],
    },
    {
      id: 'supporter', name: 'Supporter', tag: 'Thank you',
      priceM: 9.99, priceY: 79.99,
      features: ['Everything in Pro', 'Early access to features', 'Supporter badge', 'Direct line to the team'],
    },
  ];

  const tipPresets = [2, 5, 10, 25];

  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet plan-sheet ${open ? 'open' : ''}`}>
        <div className="grabber"/>
        <div className="plan-sheet-head">
          <div className="plan-sheet-tabs">
            <button className={`plan-tab ${mode === 'plans' ? 'active' : ''}`} onClick={() => setMode('plans')}>Subscription</button>
            <button className={`plan-tab ${mode === 'tip' ? 'active' : ''}`} onClick={() => setMode('tip')}>Tip jar</button>
          </div>
          <button className="nav-plain close-x" onClick={onClose} aria-label="Close">
            <Icon name="close" size={18}/>
          </button>
        </div>

        {mode === 'plans' && (
          <div className="plan-sheet-body">
            <div className="plan-billing">
              <div className="mini-seg wide">
                <button className={`mini-seg-opt ${billing === 'monthly' ? 'active' : ''}`} onClick={() => setBilling('monthly')}>Monthly</button>
                <button className={`mini-seg-opt ${billing === 'yearly' ? 'active' : ''}`} onClick={() => setBilling('yearly')}>
                  Yearly
                  <span className="save-badge">−33%</span>
                </button>
              </div>
            </div>

            <div className="plan-cards">
              {plans.map(p => (
                <button
                  key={p.id}
                  className={`plan-card ${plan === p.id ? 'current' : ''} ${p.featured ? 'featured' : ''}`}
                  onClick={() => setPlan(p.id)}
                >
                  <div className="plan-card-head">
                    <div className="plan-card-name">
                      <span className="plan-card-title">{p.name}</span>
                      {p.tag && <span className={`plan-tag ${p.featured ? 'featured' : ''}`}>{p.tag}</span>}
                    </div>
                    <div className="plan-card-price">
                      {p.priceM === 0 ? (
                        <span className="price-big">Free</span>
                      ) : (
                        <>
                          <span className="price-big">${billing === 'monthly' ? p.priceM : (p.priceY / 12).toFixed(2)}</span>
                          <span className="price-per">/mo</span>
                        </>
                      )}
                    </div>
                  </div>
                  <ul className="plan-features">
                    {p.features.map((f, i) => (
                      <li key={i}>
                        <Icon name="check" size={13}/>
                        <span>{f}</span>
                      </li>
                    ))}
                  </ul>
                  {plan === p.id ? (
                    <div className="plan-current-tag">Current plan</div>
                  ) : (
                    <div className="plan-select-tag">Select {p.name}</div>
                  )}
                </button>
              ))}
            </div>

            <button className="btn" style={{marginTop: 6}} onClick={() => { showToast(plan === 'free' ? 'Switched to Free' : `Upgraded to ${plans.find(p => p.id === plan).name}`); onClose(); }}>
              {plan === 'free' ? 'Stay on Free' : `Continue with ${plans.find(p => p.id === plan).name}`}
            </button>
            <div className="plan-legal">
              Cancel anytime. Subscription renews automatically until cancelled. Apple ID will be charged at confirmation.
            </div>
          </div>
        )}

        {mode === 'tip' && (
          <div className="plan-sheet-body">
            <div className="tip-hero">
              <div className="tip-heart">
                <svg width="40" height="40" viewBox="0 0 24 24" fill="var(--accent)">
                  <path d="M 12 20 L 3 11 Q 0 8 3 5 Q 6 2 9 4 Q 12 6 12 6 Q 12 6 15 4 Q 18 2 21 5 Q 24 8 21 11 Z"/>
                </svg>
              </div>
              <div className="tip-title">Keep Atlet moving</div>
              <div className="tip-sub">Atlet is built by a tiny team. Tips help cover servers, open-source dependencies, and the occasional coffee.</div>
            </div>

            <div className="tip-amounts">
              {tipPresets.map(a => (
                <button key={a}
                  className={`tip-preset ${tipAmount === a ? 'active' : ''}`}
                  onClick={() => setTipAmount(a)}>
                  <div className="tip-amount num">${a}</div>
                  <div className="tip-label">{a === 2 ? 'Espresso' : a === 5 ? 'Latte' : a === 10 ? 'Brunch' : 'Generous'}</div>
                </button>
              ))}
            </div>

            <div className="tip-custom">
              <label>Custom amount</label>
              <div className="tip-input-wrap">
                <span>$</span>
                <input type="number" min="1" className="tip-input num"
                  value={tipAmount}
                  onChange={(e) => setTipAmount(Number(e.target.value) || 0)}/>
              </div>
            </div>

            <button className="btn" onClick={() => { showToast(`Thanks for the $${tipAmount} tip ♥`); onClose(); }}>
              Send ${tipAmount} tip
            </button>
            <div className="plan-legal">
              One-time payment. You'll get a supporter badge on your profile for 30 days.
            </div>
          </div>
        )}
      </div>
    </>
  );
};

/* ============================================================
   Edit profile sheet — saves name/email/phone + avatar.
   "Change photo" cycles avatar color options (initials stay derived
   from the name; the color is the editable identity cue).
   ============================================================ */
const AVATAR_COLORS = ['#D2522B', '#4A7C3A', '#3A6EA5', '#8B5CF6', '#C04A6E', '#2A6B6B'];
const EditProfileSheet = ({ open, onClose, profile, onSave }) => {
  const [name, setName] = React.useState(profile.name);
  const [email, setEmail] = React.useState(profile.email);
  const [phone, setPhone] = React.useState(profile.phone || '');
  const [avatarColorIdx, setAvatarColorIdx] = React.useState(() => {
    const i = AVATAR_COLORS.indexOf(profile.avatarColor || AVATAR_COLORS[0]);
    return i >= 0 ? i : 0;
  });
  React.useEffect(() => {
    if (open) {
      setName(profile.name); setEmail(profile.email); setPhone(profile.phone || '');
      const i = AVATAR_COLORS.indexOf(profile.avatarColor || AVATAR_COLORS[0]);
      setAvatarColorIdx(i >= 0 ? i : 0);
    }
  }, [open, profile]);

  const initials = name.split(' ').map(s => s[0]).slice(0, 2).join('').toUpperCase() || '?';
  const valid = name.trim().length > 0 && /\S+@\S+\.\S+/.test(email);

  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet ${open ? 'open' : ''}`} style={{maxHeight: '78%'}}>
        <div className="grabber"/>
        <div className="sheet-head">
          <button className="nav-plain" onClick={onClose}>Cancel</button>
          <div className="sheet-title">Edit profile</div>
          <button className="nav-plain accent" disabled={!valid} onClick={() => onSave({name: name.trim(), email: email.trim(), phone, avatar: initials, avatarColor: AVATAR_COLORS[avatarColorIdx]})}>Save</button>
        </div>
        <div className="sheet-body">
          <div className="profile-edit-hero">
            {/* avatar reflects the name's initials + the picked color live */}
            <div className="profile-avatar lg" style={{background: AVATAR_COLORS[avatarColorIdx], color: '#fff'}}>{initials}</div>
            <button className="btn tinted" style={{width:'auto', padding:'6px 14px', minHeight: 32, fontSize: 13}}
              onClick={() => setAvatarColorIdx(i => (i + 1) % AVATAR_COLORS.length)}>
              Change color
            </button>
          </div>
          <div className="avatar-color-row">
            {AVATAR_COLORS.map((c, i) => (
              <button key={c} className={`avatar-color-dot ${i === avatarColorIdx ? 'active' : ''}`}
                style={{background: c}} onClick={() => setAvatarColorIdx(i)} aria-label={`Avatar color ${i+1}`}/>
            ))}
          </div>
          <div className="field">
            <label>Name</label>
            <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="Your name"/>
          </div>
          <div className="field">
            <label>Email</label>
            <input className="input" type="email" value={email} onChange={(e) => setEmail(e.target.value)} placeholder="you@example.com"/>
          </div>
          <div className="field">
            <label>Phone <span className="field-optional">(optional)</span></label>
            <input className="input" type="tel" value={phone} onChange={(e) => setPhone(e.target.value)} placeholder="+1 (555) 000-0000"/>
          </div>
        </div>
      </div>
    </>
  );
};

// Expose
Object.assign(window, { AccountView, AccountPushView, OrdersList, OrderDetail,
  SubscriptionDetail, AddressBook, AddressEditSheetInline, InvoicesList, EmptyState,
  InvoiceDetail, DonationDetail, TipsList, PaymentMethods, PaymentAddForm });

/* ============================================================
   v2 COMMERCE — push-view stack + detail screens.
   IA (GLM-5.2): Subscriptions → Orders & History → Payment & Shipping.
   The ONE botch to avoid: "a static receipt instead of interactive Order
   Details." Order Detail is a transaction dashboard (status hero, tracking
   when shipped, retry when failed); the receipt is a row INSIDE it.
   ============================================================ */

const STATUS_META = {
  confirmed: { label: 'Confirmed', tone: 'warn',  icon: 'receipt', blurb: 'We received your order.' },
  shipped:   { label: 'Shipped',   tone: 'info',  icon: 'truck',   blurb: 'On its way to you.' },
  delivered: { label: 'Delivered', tone: 'ok',    icon: 'box',     blurb: 'Enjoy! Order complete.' },
  cancelled: { label: 'Cancelled', tone: 'bad',   icon: 'close',   blurb: 'Order was cancelled.' },
};

// The router — renders the top of the nav stack with a back button.
const AccountPushView = ({ stack, onBack, orders, subscriptions, setSubscriptions,
  addresses, setAddresses, invoices, donations, paymentMethods, setPaymentMethods,
  profile, showToast, push }) => {
  const top = stack[stack.length - 1];
  const back = () => onBack();
  return (
    <div className="account-push open">
      <div className="account-nav">
        <button className="nav-plain" onClick={back}><Icon name="chevron-left" size={18}/><span>Account</span></button>
        <div className="nav-title">{top.view === 'orders' ? 'Orders' :
          top.view === 'order' ? `Order #${top.payload.number}` :
          top.view === 'subscription' ? (top.payload.tier === 'product' ? top.payload.name : top.payload.planName) :
          top.view === 'addresses' ? 'Shipping addresses' :
          top.view === 'address-edit' ? (top.payload ? 'Edit address' : 'Add address') :
          top.view === 'payment-methods' ? 'Payment methods' :
          top.view === 'payment-add' ? 'Add card' :
          top.view === 'invoice' ? `Receipt ${top.payload.number}` :
          top.view === 'donation' ? 'Tip details' :
          top.view === 'tips' ? 'Tip history' :
          'Receipts'}</div>
        <div style={{width: 60}}/>
      </div>
      <div className="account-scroll">
        {top.view === 'orders' && <OrdersList orders={orders} onOpen={(o) => push('order', o)}/>}
        {top.view === 'order' && <OrderDetail order={top.payload} onViewReceipt={() => {
          const inv = invoices.find(i => i.orderId === top.payload.id);
          if (inv) push('invoice', inv); else showToast('Receipt not found');
        }}/>}
        {top.view === 'subscription' && <SubscriptionDetail sub={top.payload} onCancel={() => {
          setSubscriptions(ss => ss.map(s => s.id === top.payload.id ? {...s, status: 'cancelled'} : s));
          showToast('Subscription cancelled');
          back();
        }}/>}
        {top.view === 'addresses' && <AddressBook addresses={addresses} setAddresses={setAddresses}
          onEdit={(a) => push('address-edit', a)} showToast={showToast}/>}
        {top.view === 'address-edit' && <AddressEditSheetInline address={top.payload}
          onSave={(a) => {
            setAddresses(aa => {
              if (a.default) aa = aa.map(x => ({...x, default: false}));
              const i = aa.findIndex(x => x.id === a.id);
              if (i >= 0) { const n = [...aa]; n[i] = a; return n; }
              return [...aa, a];
            });
            showToast(top.payload ? 'Address updated' : 'Address added');
            onBack();
          }}/>}
        {top.view === 'invoices' && <InvoicesList invoices={invoices} donations={donations} onOpenInvoice={(r) => push('invoice', r)} onOpenDonation={(d) => push('donation', d)}/>}
        {top.view === 'invoice' && <InvoiceDetail invoice={top.payload} onViewOrder={() => {
          const o = orders.find(x => x.id === top.payload.orderId);
          if (o) push('order', o); else showToast('Order not found');
        }}/>}
        {top.view === 'donation' && <DonationDetail donation={top.payload}/>}
        {top.view === 'tips' && <TipsList donations={donations} onOpen={(d) => push('donation', d)}/>}
        {top.view === 'payment-methods' && <PaymentMethods methods={paymentMethods} setMethods={setPaymentMethods} onAdd={() => push('payment-add')} showToast={showToast}/>}
        {top.view === 'payment-add' && <PaymentAddForm onAdd={(pm) => {
          setPaymentMethods(ms => {
            // first card becomes default automatically
            const makeDefault = ms.length === 0 || pm.default;
            if (makeDefault) ms = ms.map(x => ({...x, default: false}));
            return [...ms, {...pm, default: makeDefault}];
          });
          showToast('Card added');
          onBack();
        }}/>}
        <div style={{height: 24}}/>
      </div>
    </div>
  );
};

// Orders list — recent first. Row shows status, item count, total.
const OrdersList = ({ orders, onOpen }) => {
  if (!orders.length) return <EmptyState icon="box" title="No orders yet" sub="Your shop orders will show up here."/>;
  return (
    <div className="commerce-section">
      {orders.map(o => {
        const sm = STATUS_META[o.status] || STATUS_META.confirmed;
        const itemCount = o.items.reduce((n, it) => n + it.qty, 0);
        return (
          <button key={o.id} className="order-row" onClick={() => onOpen(o)}>
            <div className={`order-status ${sm.tone}`}><Icon name={sm.icon} size={13}/><span>{sm.label}</span></div>
            <div className="order-row-body">
              <div className="order-row-top">
                <span className="order-num num">#{o.number}</span>
                <span className="order-total num">{money(o.total)}</span>
              </div>
              <div className="order-row-meta">{o.date} · {itemCount} item{itemCount === 1 ? '' : 's'}</div>
            </div>
            <Icon name="chevron-right" size={14} className="order-chev"/>
          </button>
        );
      })}
    </div>
  );
};

// Order Detail — interactive transaction dashboard, NOT a static receipt.
// Status hero → items → totals breakdown → tracking (when shipped) → receipt link.
const OrderDetail = ({ order, onViewReceipt }) => {
  const sm = STATUS_META[order.status] || STATUS_META.confirmed;
  const itemCount = order.items.reduce((n, it) => n + it.qty, 0);
  return (
    <div className="commerce-section">
      {/* status hero — the focal point. Big, tone-colored, with a blurb. */}
      <div className={`order-hero ${sm.tone}`}>
        <div className="order-hero-icon"><Icon name={sm.icon} size={32}/></div>
        <div className="order-hero-label">{sm.label}</div>
        <div className="order-hero-blurb">{sm.blurb}</div>
        <div className="order-hero-num num">Order #{order.number}</div>
      </div>

      {/* tracking — prominent tappable link when shipped (GLM-5.2: the botch-avoidance) */}
      {order.status === 'shipped' && (
        <button className="tracking-card" onClick={() => {}}>
          <div className="tracking-left"><Icon name="truck" size={18}/><div><div className="tracking-title">Track shipment</div><div className="tracking-sub num">1Z999AA10123456784</div></div></div>
          <Icon name="chevron-right" size={14}/>
        </button>
      )}

      <ListGroup header="Items">
        {order.items.map((it, i) => (
          <div key={i} className="line-item">
            <div className="line-item-body">
              <div className="line-item-name">{it.name}</div>
              <div className="line-item-qty">Qty {it.qty}</div>
            </div>
            <div className="line-item-price num">{money(it.price * it.qty)}</div>
          </div>
        ))}
      </ListGroup>

      {/* totals breakdown — the structured money view */}
      <ListGroup header="Total">
        <div className="totals-row"><span>Subtotal</span><span className="num">{money(order.subtotal)}</span></div>
        <div className="totals-row"><span>Shipping</span><span className="num">{order.shipping === 0 ? 'Free' : money(order.shipping)}</span></div>
        <div className="totals-row"><span>Tax</span><span className="num">{money(order.tax)}</span></div>
        <div className="totals-row totals-grand"><span>Total</span><span className="num">{money(order.total)}</span></div>
        <div className="totals-paid"><Icon name="check" size={12}/> Paid {order.date} · {itemCount} item{itemCount === 1 ? '' : 's'}</div>
      </ListGroup>

      <ListGroup header="Receipt">
        <Row icon={<Icon name="receipt" size={17}/>} title="View receipt (PDF)"
          detail={`Invoice #inv-${order.number}`} onTap={onViewReceipt} chevron/>
        <Row icon={<Icon name="help" size={17}/>} title="Something wrong with this order?"
          onTap={() => {}} chevron/>
      </ListGroup>
    </div>
  );
};

// Subscription Detail — plan/price/cadence/next-billing + cancel (destructive, not buried).
const SubscriptionDetail = ({ sub, onCancel }) => {
  const cancelled = sub.status === 'cancelled';
  return (
    <div className="commerce-section">
      <div className={`sub-hero ${cancelled ? 'dim' : ''}`}>
        <div className="sub-hero-name">{sub.tier === 'product' ? sub.name : sub.planName}</div>
        <div className="sub-hero-price num">{money(sub.price)}<span className="sub-hero-cad">/{sub.cadence === 'yearly' ? 'yr' : 'mo'}</span></div>
        <div className={`sub-status ${cancelled ? 'bad' : 'ok'}`}>{cancelled ? 'Cancelled' : 'Active'}</div>
      </div>
      <ListGroup header="Plan details">
        <div className="totals-row"><span>Type</span><span>{sub.tier === 'product' ? 'Product refill' : sub.tier === 'app' ? 'App membership' : 'Subscription'}</span></div>
        <div className="totals-row"><span>Cadence</span><span className="num">{sub.cadence === 'yearly' ? 'Yearly' : 'Monthly'}</span></div>
        {!cancelled && sub.nextDate && <div className="totals-row"><span>Next billing</span><span className="num">{sub.nextDate}</span></div>}
      </ListGroup>
      {!cancelled ? (
        <>
          <button className="btn ghost" style={{marginTop: 4}} onClick={onCancel}>Cancel subscription</button>
          <div className="plan-legal">You'll keep access until {sub.nextDate}. Cancel anytime — no fee.</div>
        </>
      ) : (
        <div className="plan-legal">This subscription is cancelled. You won't be billed again.</div>
      )}
    </div>
  );
};

// Address Book — list with default checkmark + add.
const AddressBook = ({ addresses, setAddresses, onEdit, showToast }) => {
  const setDefault = (id) => {
    setAddresses(aa => aa.map(a => ({...a, default: a.id === id})));
    showToast('Default address updated');
  };
  return (
    <div className="commerce-section">
      {addresses.length === 0 && <EmptyState icon="pin" title="No addresses" sub="Add a shipping address to check out faster."/>}
      {addresses.map(a => (
        <div key={a.id} className={`address-row ${a.default ? 'default' : ''}`}>
          <button className="address-row-radio" onClick={() => setDefault(a.id)} aria-label="Set default">
            {a.default && <Icon name="check" size={12} stroke={3}/>}
          </button>
          <button className="address-row-body" onClick={() => onEdit(a)}>
            <div className="address-row-label">{a.label}{a.default && <span className="default-pill">Default</span>}</div>
            <div className="address-row-name">{a.name}</div>
            <div className="address-row-lines">{a.line1}{a.line2 ? `, ${a.line2}` : ''}, {a.city}, {a.state} {a.postal}</div>
          </button>
          <button className="address-row-edit" onClick={() => onEdit(a)} aria-label="Edit"><Icon name="edit" size={15}/></button>
        </div>
      ))}
      <button className="btn tinted add-address-btn" onClick={() => onEdit(null)}><Icon name="plus" size={16}/> Add address</button>
    </div>
  );
};

// Address edit/add — rendered inline (not a sheet) inside the push stack.
const AddressEditSheetInline = ({ address, onSave }) => {
  const isEdit = !!address;
  const [form, setForm] = React.useState(address || {
    id: 'addr-' + Date.now(), label: 'Home', default: false,
    name: '', line1: '', line2: '', city: '', state: '', postal: '', country: 'United States',
  });
  const upd = (k, v) => setForm(f => ({...f, [k]: v}));
  const valid = form.name && form.line1 && form.city && form.postal;
  return (
    <div className="commerce-section address-form">
      <ListGroup header="Label">
        <div className="label-pills">
          {['Home', 'Work', 'Other'].map(l => (
            <button key={l} className={`label-pill ${form.label === l ? 'active' : ''}`} onClick={() => upd('label', l)}>{l}</button>
          ))}
        </div>
      </ListGroup>
      <ListGroup header="Address">
        <div className="field-stack">
          <div className="field"><label>Full name</label><input className="input" value={form.name} onChange={(e) => upd('name', e.target.value)} placeholder="Recipient name"/></div>
          <div className="field"><label>Address line 1</label><input className="input" value={form.line1} onChange={(e) => upd('line1', e.target.value)} placeholder="Street address"/></div>
          <div className="field"><label>Address line 2 <span className="field-optional">(optional)</span></label><input className="input" value={form.line2} onChange={(e) => upd('line2', e.target.value)} placeholder="Apt, suite, unit"/></div>
          <div className="field-row">
            <div className="field"><label>City</label><input className="input" value={form.city} onChange={(e) => upd('city', e.target.value)} placeholder="City"/></div>
            <div className="field field-sm"><label>Postal</label><input className="input" value={form.postal} onChange={(e) => upd('postal', e.target.value)} placeholder="ZIP"/></div>
          </div>
          <div className="field"><label>Country</label><input className="input" value={form.country} onChange={(e) => upd('country', e.target.value)}/></div>
        </div>
      </ListGroup>
      <button className="btn" disabled={!valid} onClick={() => onSave({...form, default: form.default || !isEdit && false})}>
        {isEdit ? 'Save changes' : 'Add address'}
      </button>
      <div className="address-make-default">
        <button className={`ios-switch ${form.default ? 'on' : ''}`} onClick={() => upd('default', !form.default)} aria-label="Make default"/>
        <span>Set as default shipping address</span>
      </div>
    </div>
  );
};

// Invoices / Receipts — all financial records (order + sub + donation).
const InvoicesList = ({ invoices, donations, onOpenInvoice, onOpenDonation }) => {
  // merge invoices + donations (donations are receipts too) into one timeline.
  // Each row carries its ref so the drill-in opens the right detail view.
  const all = [
    ...invoices.map(i => ({ ref: i, id: i.id, date: i.date, number: i.number, amount: i.amount, kind: i.kind, status: i.status })),
    ...donations.map(d => ({ ref: d, id: d.id, date: d.date, number: 'don-' + d.id, amount: d.amount, kind: 'donation', status: 'paid' })),
  ].sort((a, b) => (a.date < b.date ? 1 : -1));
  const KIND = {
    order: { label: 'Order', icon: 'box' },
    product: { label: 'Order', icon: 'box' },
    subscription: { label: 'Subscription', icon: 'sync' },
    app: { label: 'Membership', icon: 'sync' },
    donation: { label: 'Donation', icon: 'heart' },
  };
  if (!all.length) return <EmptyState icon="receipt" title="No receipts yet" sub="Invoices for orders, subscriptions, and tips appear here."/>;
  return (
    <div className="commerce-section">
      {all.map(r => {
        const k = KIND[r.kind] || KIND.order;
        return (
          <button key={r.id} className="invoice-row" onClick={() => r.kind === 'donation' ? onOpenDonation(r.ref) : onOpenInvoice(r.ref)}>
            <div className="invoice-icon"><Icon name={k.icon} size={16}/></div>
            <div className="invoice-body">
              <div className="invoice-top"><span className="invoice-num num">{r.number}</span><span className="invoice-amount num">{money(r.amount)}</span></div>
              <div className="invoice-meta">{r.date} · {k.label} · <span className="invoice-paid">Paid</span></div>
            </div>
            <Icon name="chevron-right" size={14} className="invoice-chev"/>
          </button>
        );
      })}
    </div>
  );
};

const EmptyState = ({ icon, title, sub }) => (
  <div className="commerce-empty">
    <div className="commerce-empty-icon"><Icon name={icon} size={28}/></div>
    <div className="commerce-empty-title">{title}</div>
    <div className="commerce-empty-sub">{sub}</div>
  </div>
);

/* --- Invoice / Receipt detail (GLM-5.2: line items + tax breakdown + PDF + linked order) --- */
const KIND_LABEL = { product: 'Order', order: 'Order', subscription: 'Subscription', app: 'Membership', donation: 'Donation' };
const InvoiceDetail = ({ invoice, onViewOrder }) => {
  const hasBreakdown = invoice.subtotal != null;
  return (
    <div className="commerce-section">
      <div className="receipt-hero">
        <div className="receipt-hero-glyph"><Icon name="receipt" size={28}/></div>
        <div className="receipt-hero-amount num">{money(invoice.total != null ? invoice.total : invoice.amount)}</div>
        <div className="receipt-hero-meta">Receipt {invoice.number} · {KIND_LABEL[invoice.kind] || 'Receipt'}</div>
        <div className={`receipt-status ${invoice.status === 'paid' ? 'ok' : 'warn'}`}>{invoice.status === 'paid' ? 'Paid' : invoice.status}</div>
        <div className="receipt-hero-date">{invoice.date} · {invoice.method}</div>
      </div>
      {hasBreakdown && invoice.items && (
        <ListGroup header="Items">
          {invoice.items.map((it, i) => (
            <div key={i} className="line-item">
              <div className="line-item-body">
                <div className="line-item-name">{it.name}</div>
                <div className="line-item-qty">Qty {it.qty}</div>
              </div>
              <div className="line-item-price num">{money(it.price * it.qty)}</div>
            </div>
          ))}
        </ListGroup>
      )}
      {hasBreakdown && (
        <ListGroup header="Total">
          <div className="totals-row"><span>Subtotal</span><span className="num">{money(invoice.subtotal)}</span></div>
          <div className="totals-row"><span>Shipping</span><span className="num">{invoice.shipping === 0 ? 'Free' : money(invoice.shipping)}</span></div>
          <div className="totals-row"><span>Tax</span><span className="num">{money(invoice.tax)}</span></div>
          <div className="totals-row totals-grand"><span>Total</span><span className="num">{money(invoice.total)}</span></div>
        </ListGroup>
      )}
      <ListGroup header="Actions">
        <Row icon={<Icon name="download" size={17}/>} title="Download PDF" detail={`Receipt ${invoice.number}`} onTap={() => {}} chevron/>
        {invoice.orderId && <Row icon={<Icon name="box" size={17}/>} title="View order" detail="Jump to this order" onTap={onViewOrder} chevron/>}
        <Row icon={<Icon name="help" size={17}/>} title="Question about this charge?" onTap={() => {}} chevron/>
      </ListGroup>
    </div>
  );
};

/* --- Donation / tip detail --- */
const DonationDetail = ({ donation }) => (
  <div className="commerce-section">
    <div className="donation-hero">
      <div className="donation-hero-glyph"><Icon name="heart" size={30}/></div>
      <div className="donation-hero-amount num">{money(donation.amount)}</div>
      <div className="donation-hero-kind">{donation.recurring ? 'Monthly tip' : 'One-time tip'}</div>
      <div className="donation-hero-date">{donation.date} · {donation.method || 'Visa •• 4242'}</div>
      <div className="receipt-status ok">Paid · Tax-deductible</div>
    </div>
    {donation.message ? (
      <ListGroup header="Your message">
        <div className="donation-message">"{donation.message}"</div>
      </ListGroup>
    ) : (
      <div className="plan-legal">No message was left with this tip. 100% of it funds Atlet's development.</div>
    )}
    <ListGroup header="Actions">
      <Row icon={<Icon name="receipt" size={17}/>} title="View receipt" detail={`don-${donation.id}`} onTap={() => {}} chevron/>
      <Row icon={<Icon name="download" size={17}/>} title="Download receipt" onTap={() => {}} chevron/>
    </ListGroup>
  </div>
);

/* --- Tips list (donations, with detail drill-in) --- */
const TipsList = ({ donations, onOpen }) => {
  if (!donations.length) return <EmptyState icon="heart" title="No tips yet" sub="When you tip to support Atlet, your history appears here."/>;
  const total = donations.reduce((s, d) => s + d.amount, 0);
  return (
    <div className="commerce-section">
      <div className="tips-summary">
        <div className="tips-summary-amount num">{money(total)}</div>
        <div className="tips-summary-label">total tipped · {donations.length} {donations.length === 1 ? 'gift' : 'gifts'}</div>
      </div>
      {donations.map(d => (
        <button key={d.id} className="order-row" onClick={() => onOpen(d)}>
          <div className="order-status info"><Icon name="heart" size={13}/><span>Tip</span></div>
          <div className="order-row-body">
            <div className="order-row-top">
              <span className="order-num">{d.recurring ? 'Monthly tip' : 'One-time tip'}</span>
              <span className="order-total num">{money(d.amount)}</span>
            </div>
            <div className="order-row-meta">{d.date}{d.message ? ' · ' + d.message : ''}</div>
          </div>
          <Icon name="chevron-right" size={14} className="order-chev"/>
        </button>
      ))}
    </div>
  );
};

/* --- Payment methods — list + set-default + delete (default-reassign guard) + expired state --- */
const BRAND_GLYPH = {
  visa: 'VISA', mastercard: 'MC', amex: 'AMEX', discover: 'DISC',
};
const isExpired = (pm) => {
  const now = new Date();
  const exp = new Date(pm.expYear, pm.expMonth - 1, 1);
  return exp < new Date(now.getFullYear(), now.getMonth(), 1);
};
const PaymentMethods = ({ methods, setMethods, onAdd, showToast }) => {
  const setDefault = (id) => {
    setMethods(ms => ms.map(m => ({...m, default: m.id === id})));
    showToast('Default card updated');
  };
  const remove = (id) => {
    setMethods(ms => {
      const target = ms.find(m => m.id === id);
      let next = ms.filter(m => m.id !== id);
      // GLM-5.2 guard: deleting the default reassigns to the next available card
      if (target && target.default && next.length) next[0] = {...next[0], default: true};
      return next;
    });
    showToast('Card removed');
  };
  if (!methods.length) return (
    <EmptyState icon="card" title="No payment methods" sub="Add a card to check out faster and manage subscriptions."/>);
  return (
    <div className="commerce-section">
      {methods.map(m => {
        const expired = isExpired(m);
        return (
          <div key={m.id} className={`payment-card ${m.default ? 'default' : ''} ${expired ? 'expired' : ''}`}>
            <button className="payment-card-radio" onClick={() => !expired && setDefault(m.id)} disabled={expired} aria-label="Set default">
              {m.default && <Icon name="check" size={12} stroke={3}/>}
            </button>
            <div className="payment-card-body">
              <div className="payment-card-top">
                <span className="payment-brand">{BRAND_GLYPH[m.brand] || m.brand.toUpperCase()}</span>
                {expired && <span className="payment-expired-tag">Expired</span>}
                {m.default && <span className="default-pill">Default</span>}
              </div>
              <div className="payment-card-num num">•••• •••• •••• {m.last4}</div>
              <div className="payment-card-name">{m.name}</div>
            </div>
            <div className="payment-card-foot">
              <span className="payment-exp num">{String(m.expMonth).padStart(2,'0')}/{String(m.expYear).slice(-2)}</span>
              <button className="payment-delete" onClick={() => remove(m.id)} aria-label="Remove card"><Icon name="trash" size={15}/></button>
            </div>
          </div>
        );
      })}
      <button className="btn tinted add-address-btn" onClick={onAdd}><Icon name="plus" size={16}/> Add card</button>
      <div className="plan-legal">Cards are securely tokenized via Stripe. We never store your CVC.</div>
    </div>
  );
};

/* --- Add card form (inline in the push stack) --- */
const PaymentAddForm = ({ onAdd }) => {
  const [form, setForm] = React.useState({ brand: 'visa', number: '', name: '', expMonth: '', expYear: '', cvc: '', default: false });
  const upd = (k, v) => setForm(f => ({...f, [k]: v}));
  // brand inferred from number prefix; last4 derived; basic 16-digit check
  const detectBrand = (num) => {
    const n = num.replace(/\s/g, '');
    if (/^4/.test(n)) return 'visa';
    if (/^5[1-5]/.test(n)) return 'mastercard';
    if (/^3[47]/.test(n)) return 'amex';
    if (/^6/.test(n)) return 'discover';
    return 'visa';
  };
  const cleanNum = (form.number || '').replace(/\D/g, '');
  const valid = cleanNum.length >= 15 && form.name && form.expMonth && form.expYear && form.cvc.length >= 3;
  const submit = () => onAdd({
    id: 'pm-' + Date.now(),
    brand: detectBrand(form.number),
    last4: cleanNum.slice(-4),
    name: form.name,
    expMonth: Number(form.expMonth), expYear: Number(form.expYear.length === 2 ? '20' + form.expYear : form.expYear),
    default: form.default,
  });
  return (
    <div className="commerce-section address-form">
      <ListGroup header="Card details">
        <div className="field-stack">
          <div className="field"><label>Card number</label>
            <input className="input" inputMode="numeric" placeholder="4242 4242 4242 4242" value={form.number}
              onChange={(e) => upd('number', e.target.value)}/>
          </div>
          <div className="field"><label>Name on card</label>
            <input className="input" value={form.name} onChange={(e) => upd('name', e.target.value)} placeholder="Cardholder name"/>
          </div>
          <div className="field-row-3">
            <div className="field"><label>MM</label><input className="input" inputMode="numeric" maxLength={2} placeholder="11" value={form.expMonth} onChange={(e) => upd('expMonth', e.target.value)}/></div>
            <div className="field"><label>YY</label><input className="input" inputMode="numeric" maxLength={2} placeholder="27" value={form.expYear} onChange={(e) => upd('expYear', e.target.value)}/></div>
            <div className="field"><label>CVC</label><input className="input" inputMode="numeric" maxLength={4} placeholder="123" value={form.cvc} onChange={(e) => upd('cvc', e.target.value)}/></div>
          </div>
        </div>
      </ListGroup>
      <button className="btn" disabled={!valid} onClick={submit}>Add card</button>
      <div className="address-make-default">
        <button className={`ios-switch ${form.default ? 'on' : ''}`} onClick={() => upd('default', !form.default)} aria-label="Make default"/>
        <span>Set as default payment method</span>
      </div>
    </div>
  );
};
