// auth.jsx — splash, sign-in, OTP flow

// ===== Atlet logo mark =====
// A tilted "A" formed by two strokes intersecting a horizontal line — evokes a runner's stride.
const AtletMark = ({ size = 80, stroke = '#1A1714', accent = 'var(--accent)' }) => (
  <svg width={size} height={size} viewBox="0 0 80 80" fill="none" style={{display:'block'}}>
    <defs>
      <linearGradient id="atl-g" x1="0" y1="0" x2="1" y2="1">
        <stop offset="0%" stopColor={accent} stopOpacity="0"/>
        <stop offset="100%" stopColor={accent} stopOpacity="0.18"/>
      </linearGradient>
    </defs>
    <rect x="2" y="2" width="76" height="76" rx="22" fill="url(#atl-g)" stroke={accent} strokeOpacity="0.25" strokeWidth="1"/>
    {/* left stroke of A */}
    <path d="M22 62 L40 18" stroke={stroke} strokeWidth="6" strokeLinecap="round"/>
    {/* right stroke of A (accent) */}
    <path d="M40 18 L58 62" stroke={accent} strokeWidth="6" strokeLinecap="round"/>
    {/* crossbar — tilted like a sprinter's lean */}
    <path d="M28 48 L54 42" stroke={stroke} strokeWidth="5" strokeLinecap="round"/>
  </svg>
);

const AtletWordmark = ({ color = 'var(--ink)', size = 32 }) => (
  <div style={{
    fontFamily: 'var(--display, Fraunces), serif',
    fontSize: size,
    fontWeight: 400,
    letterSpacing: '-0.04em',
    color,
    lineHeight: 1,
  }}>
    Atlet<span style={{color: 'var(--accent)'}}>.</span>
  </div>
);

// ===== Splash =====
const SplashView = ({ onReady }) => {
  const [progress, setProgress] = React.useState(0);

  React.useEffect(() => {
    const start = performance.now();
    const DUR = 1800;
    const id = setInterval(() => {
      const p = Math.min(1, (performance.now() - start) / DUR);
      setProgress(p);
      if (p >= 1) { clearInterval(id); setTimeout(onReady, 320); }
    }, 30);
    return () => clearInterval(id);
  }, []);

  return (
    <div className="auth-view splash">
      <div className="splash-center">
        <div className="splash-logo rise" style={{animationDelay:'60ms'}}>
          <AtletMark size={80}/>
        </div>
        <div className="caption rise" style={{animationDelay:'260ms', marginTop: 14, color:'var(--ink-3)', letterSpacing:'0.28em', textTransform:'uppercase', fontSize:10}}>
          move everyday
        </div>
      </div>
      <div className="splash-bottom">
        <div className="loader-track">
          <div className="loader-fill" style={{width: `${progress * 100}%`}}/>
        </div>
        <div className="caption num" style={{marginTop: 10, color:'var(--ink-4)', fontSize:10, letterSpacing:'0.14em', textTransform:'uppercase', textAlign:'right'}}>
          Loading · {Math.round(progress * 100)}%
        </div>
      </div>
    </div>
  );
};

// ===== Sign-in =====
const GoogleGlyph = () => (
  <svg width="18" height="18" viewBox="0 0 18 18">
    <path d="M17.64 9.2c0-.64-.06-1.25-.16-1.84H9v3.48h4.84a4.14 4.14 0 01-1.8 2.72v2.26h2.91c1.7-1.57 2.69-3.88 2.69-6.62z" fill="#4285F4"/>
    <path d="M9 18c2.43 0 4.47-.81 5.96-2.18l-2.91-2.26c-.81.54-1.84.86-3.05.86-2.34 0-4.33-1.58-5.04-3.7H.96v2.33A9 9 0 009 18z" fill="#34A853"/>
    <path d="M3.96 10.72a5.4 5.4 0 010-3.44V4.95H.96a9 9 0 000 8.1l3-2.33z" fill="#FBBC04"/>
    <path d="M9 3.58c1.32 0 2.5.45 3.44 1.35l2.58-2.59C13.46.89 11.43 0 9 0A9 9 0 00.96 4.95l3 2.33C4.67 5.16 6.66 3.58 9 3.58z" fill="#EA4335"/>
  </svg>
);

const AppleGlyph = () => (
  <svg width="18" height="18" viewBox="0 0 18 18" fill="currentColor">
    <path d="M13.17 9.56c-.02-2.07 1.7-3.07 1.78-3.12-.97-1.42-2.47-1.61-3.01-1.63-1.27-.13-2.5.76-3.14.76-.66 0-1.66-.75-2.74-.72-1.4.02-2.7.81-3.42 2.07-1.47 2.55-.37 6.3 1.05 8.37.7 1.01 1.5 2.14 2.58 2.1 1.04-.04 1.43-.67 2.68-.67 1.25 0 1.61.67 2.7.65 1.12-.02 1.83-1.02 2.51-2.04.79-1.17 1.11-2.31 1.13-2.37-.03-.01-2.16-.83-2.18-3.29M11.21 3.46c.57-.69.96-1.66.85-2.62-.82.03-1.82.55-2.41 1.24-.53.61-.99 1.59-.87 2.53.92.07 1.85-.46 2.43-1.15"/>
  </svg>
);

const SignInView = ({ onSubmit }) => {
  const [email, setEmail] = React.useState('');
  const [working, setWorking] = React.useState(null); // 'google' | 'apple' | 'email'

  const valid = /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);

  const go = (mode) => {
    setWorking(mode);
    setTimeout(() => {
      setWorking(null);
      onSubmit(mode, email);
    }, 780);
  };

  return (
    <div className="auth-view">
      <div className="auth-body">
        <div className="rise auth-hero" style={{animationDelay:'20ms'}}>
          <AtletMark size={80}/>
        </div>
        <div className="eyebrow rise" style={{animationDelay:'60ms', color:'var(--accent)'}}>Welcome</div>
        <h1 className="auth-h rise" style={{animationDelay:'120ms'}}>Move everyday.</h1>
        <p className="auth-sub rise" style={{animationDelay:'180ms'}}>Log your workouts, build streaks, and keep your rhythm.</p>

        <div className="auth-buttons rise" style={{animationDelay:'260ms'}}>
          <button className="auth-btn social google" onClick={() => go('google')} disabled={!!working}>
            {working === 'google' ? <Spinner/> : <><GoogleGlyph/><span>Continue with Google</span></>}
          </button>
          <button className="auth-btn social apple" onClick={() => go('apple')} disabled={!!working}>
            {working === 'apple' ? <Spinner light/> : <><AppleGlyph/><span>Continue with Apple</span></>}
          </button>
        </div>

        <div className="auth-divider rise" style={{animationDelay:'320ms'}}>
          <span>or use email</span>
        </div>

        <div className="auth-email rise" style={{animationDelay:'380ms'}}>
          <input
            type="email"
            inputMode="email"
            autoComplete="email"
            placeholder="you@example.com"
            value={email}
            onChange={e => setEmail(e.target.value)}
            className="auth-input"
          />
          <button
            className="auth-btn primary"
            disabled={!valid || !!working}
            onClick={() => go('email')}
          >
            {working === 'email' ? <Spinner light/> : <>Send code <Icon name="back" size={14} style={{transform:'rotate(180deg)', marginLeft: 6}}/></>}
          </button>
        </div>
      </div>

      <div className="auth-foot rise" style={{animationDelay:'460ms'}}>
        By continuing you agree to Atlet's <u>Terms</u> and <u>Privacy</u>.
      </div>
    </div>
  );
};

const Spinner = ({ light }) => (
  <div className={`spinner ${light ? 'light' : ''}`}>
    <svg width="18" height="18" viewBox="0 0 18 18">
      <circle cx="9" cy="9" r="7" stroke="currentColor" strokeOpacity="0.2" strokeWidth="2" fill="none"/>
      <circle cx="9" cy="9" r="7" stroke="currentColor" strokeWidth="2" fill="none"
        strokeLinecap="round" strokeDasharray="44" strokeDashoffset="30"
        style={{transformOrigin:'center', animation:'spin 0.9s linear infinite'}}/>
    </svg>
  </div>
);

// ===== OTP =====
const OTPView = ({ email, onBack, onVerify }) => {
  const [digits, setDigits] = React.useState(['', '', '', '', '', '']);
  const [err, setErr] = React.useState(false);
  const [cooldown, setCooldown] = React.useState(30);
  const [verifying, setVerifying] = React.useState(false);
  const inputsRef = React.useRef([]);

  React.useEffect(() => {
    inputsRef.current[0]?.focus();
  }, []);

  React.useEffect(() => {
    if (cooldown <= 0) return;
    const id = setTimeout(() => setCooldown(c => c - 1), 1000);
    return () => clearTimeout(id);
  }, [cooldown]);

  const setDigit = (i, v) => {
    const clean = v.replace(/\D/g, '').slice(-1);
    const next = [...digits];
    next[i] = clean;
    setDigits(next);
    setErr(false);
    if (clean && i < 5) inputsRef.current[i + 1]?.focus();
    // auto-verify when all filled
    if (clean && i === 5 && next.every(d => d !== '')) {
      submit(next.join(''));
    }
  };

  const onKey = (i, e) => {
    if (e.key === 'Backspace' && !digits[i] && i > 0) {
      inputsRef.current[i - 1]?.focus();
    }
  };

  const onPaste = (e) => {
    const txt = e.clipboardData.getData('text').replace(/\D/g,'').slice(0, 6);
    if (!txt) return;
    e.preventDefault();
    const next = txt.split('').concat(Array(6).fill('')).slice(0, 6);
    setDigits(next);
    if (next.every(d => d !== '')) submit(txt);
    else inputsRef.current[Math.min(txt.length, 5)]?.focus();
  };

  const submit = (code) => {
    setVerifying(true);
    setTimeout(() => {
      // Demo: any 6-digit code works, except "000000"
      if (code === '000000') {
        setErr(true);
        setDigits(['','','','','','']);
        setVerifying(false);
        inputsRef.current[0]?.focus();
      } else {
        onVerify();
      }
    }, 620);
  };

  const resend = () => {
    if (cooldown > 0) return;
    setCooldown(30);
    setDigits(['','','','','','']);
    setErr(false);
    inputsRef.current[0]?.focus();
  };

  return (
    <div className="auth-view">
      <div className="auth-top">
        <button className="icon-btn" onClick={onBack}><Icon name="back" size={18}/></button>
        <AtletMark size={34}/>
        <div style={{width: 40}}/>
      </div>

      <div className="auth-body">
        <div className="eyebrow rise" style={{animationDelay:'20ms', color:'var(--accent)'}}>Verify email</div>
        <h1 className="auth-h rise" style={{animationDelay:'80ms'}}>Enter your code.</h1>
        <p className="auth-sub rise" style={{animationDelay:'140ms'}}>
          We sent a 6-digit code to <strong style={{color:'var(--ink)'}}>{email || 'your email'}</strong>.
        </p>

        <div className={`otp-row rise ${err ? 'shake' : ''}`} style={{animationDelay:'220ms'}} onPaste={onPaste}>
          {digits.map((d, i) => (
            <input
              key={i}
              ref={el => inputsRef.current[i] = el}
              className={`otp-cell ${d ? 'filled' : ''} ${err ? 'err' : ''}`}
              value={d}
              onChange={e => setDigit(i, e.target.value)}
              onKeyDown={e => onKey(i, e)}
              inputMode="numeric"
              maxLength={1}
              disabled={verifying}
            />
          ))}
        </div>

        {err && (
          <div className="caption" style={{color:'var(--accent)', fontSize:12, marginTop: 2, animation:'flashIn 0.3s'}}>
            That code didn't match. Try again.
          </div>
        )}
        {verifying && !err && (
          <div className="caption" style={{color:'var(--ink-3)', fontSize:12, marginTop: 2, display:'flex', alignItems:'center', gap:8}}>
            <Spinner/> Checking…
          </div>
        )}

        <div className="otp-resend rise" style={{animationDelay:'320ms'}}>
          <span>Didn't get it?</span>
          <button
            className={`resend-btn ${cooldown > 0 ? 'disabled' : ''}`}
            onClick={resend}
            disabled={cooldown > 0}
          >
            {cooldown > 0 ? `Resend in ${cooldown}s` : 'Resend code'}
          </button>
        </div>

        <div className="caption" style={{marginTop: 14, color:'var(--ink-4)', fontSize:11}}>
          Hint — any 6 digits will work in this demo (try <code style={{fontFamily:'var(--mono)'}}>000000</code> to see the error state).
        </div>
      </div>
    </div>
  );
};

// ===== Welcome pulse =====
const WelcomeView = ({ name, onDone }) => {
  React.useEffect(() => {
    const t = setTimeout(onDone, 1400);
    return () => clearTimeout(t);
  }, []);

  return (
    <div className="auth-view welcome">
      <div className="welcome-center">
        <div className="welcome-ring"/>
        <div className="welcome-check">
          <Icon name="check" size={44}/>
        </div>
      </div>
      <div className="welcome-text">
        <h1 className="auth-h" style={{fontSize: 30, margin: 0}}>Welcome{name ? `, ${name}` : ''}.</h1>
        <p className="auth-sub" style={{margin: 0}}>Let's get moving.</p>
      </div>
    </div>
  );
};

// Export
Object.assign(window, { AtletMark, AtletWordmark, SplashView, SignInView, OTPView, WelcomeView });
