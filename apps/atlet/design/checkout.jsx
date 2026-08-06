// checkout.jsx — v2 ADDITION. Unified checkout for cart / subscription / donation.
// Mirrors Stripe Checkout Session semantics (mode:'payment' vs 'subscription').

const Checkout = ({ checkout, onClose, onComplete, cart, addresses, profile }) => {
  const [step, setStep] = React.useState(0);
  const [addressId, setAddressId] = React.useState(addresses.find(a => a.default)?.id || addresses[0]?.id);
  const [shipRateId, setShipRateId] = React.useState('ship-standard');
  const [billing, setBilling] = React.useState('monthly');
  const [processing, setProcessing] = React.useState(false);
  React.useEffect(() => { if (checkout) { setStep(0); setProcessing(false); } }, [checkout]);
  const open = !!checkout;
  if (!open) return null;

  const needsShipping = checkout.kind === 'cart';
  let summary = { rows: [], subtotal: 0, discount: 0, shipping: 0, tax: 0, total: 0, hasShipping: false, cta: '' };
  if (checkout.kind === 'cart') {
    const lines = cart.map(l => { const p = SEED_PRODUCTS.find(x => x.id === l.productId); return { name: p.name, detail: p.size, qty: l.qty, amount: p.price * l.qty }; });
    const subtotal = lines.reduce((s, l) => s + l.amount, 0);
    const discount = profile.plan === 'supporter' ? subtotal * 0.15 : profile.plan === 'pro' ? subtotal * 0.10 : 0;
    const rate = resolveShipping(subtotal, shipRateId);
    const tax = (subtotal - discount) * TAX_RATE;
    summary = { rows: lines, subtotal, discount, shipping: rate.price, tax, total: subtotal - discount + rate.price + tax, hasShipping: true, cta: `Pay ${money(subtotal - discount + rate.price + tax)}` };
  } else if (checkout.kind === 'subscription' && checkout.tier === 'app') {
    const plan = APP_PLANS.find(p => p.id === checkout.planId);
    const price = billing === 'yearly' ? plan.priceY : plan.priceM;
    summary = { rows: [{ name: `${plan.name} membership`, detail: billing === 'yearly' ? 'Billed annually' : 'Billed monthly', qty: 1, amount: price }], subtotal: price, discount: 0, shipping: 0, tax: 0, total: price, hasShipping: false, cta: price === 0 ? 'Start free' : `Subscribe · ${money(price)}/${billing === 'yearly' ? 'yr' : 'mo'}` };
  } else if (checkout.kind === 'donation') {
    summary = { rows: [{ name: 'One-time tip', detail: 'Thank you', qty: 1, amount: checkout.amount }], subtotal: checkout.amount, discount: 0, shipping: 0, tax: 0, total: checkout.amount, hasShipping: false, cta: `Donate ${money(checkout.amount)}` };
  }
  const steps = needsShipping ? ['Review', 'Shipping', 'Payment'] : ['Review', 'Payment'];
  const currentTitle = steps[step];
  const pay = () => { setProcessing(true); setTimeout(() => { onComplete({ addressId, shipRateId, billing }); setProcessing(false); }, 1200); };
  const goNext = () => setStep(s => Math.min(steps.length - 1, s + 1));
  const goBack = () => step === 0 ? onClose() : setStep(s => s - 1);

  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet checkout-sheet ${open ? 'open' : ''}`}>
        <div className="grabber"/>
        <div className="checkout-head">
          <button className="nav-plain" onClick={goBack}><Icon name="chevron-left" size={18}/>{step === 0 ? 'Cancel' : 'Back'}</button>
          <div className="sheet-title">{currentTitle}</div>
          <div className="checkout-steps">{steps.map((s, i) => <span key={i} className={`checkout-step ${i === step ? 'active' : ''} ${i < step ? 'done' : ''}`}/>)}</div>
        </div>
        <div className="checkout-body">
          {step === 0 && (<>
            <div className="checkout-merchant"><AtletMark size={28}/><div><div className="checkout-merchant-name">Atlet</div><div className="checkout-merchant-sub">secure · powered by Stripe</div></div><Icon name="lock" size={14}/></div>
            <div className="list-header" style={{paddingLeft: 0}}>Order</div>
            <div className="checkout-items">
              {summary.rows.map((r, i) => (
                <div key={i} className="checkout-item"><div className="checkout-item-body"><div className="checkout-item-name">{r.name}</div><div className="checkout-item-detail">{r.detail}</div></div>{r.qty > 1 && <span className="num muted">×{r.qty}</span>}<span className="num">{money(r.amount)}</span></div>
              ))}
            </div>
            <CheckoutTotals summary={summary}/>
            {needsShipping ? <button className="checkout-cta" onClick={goNext}>Continue to shipping <Icon name="arrow-right" size={16}/></button> : <button className="btn" disabled={processing} onClick={pay}>{processing ? 'Processing…' : summary.cta}</button>}
          </>)}
          {needsShipping && step === 1 && (<>
            <div className="list-header" style={{paddingLeft: 0}}>Ship to</div>
            <div className="checkout-addresses">
              {addresses.map(a => (
                <button key={a.id} className={`address-card ${addressId === a.id ? 'selected' : ''}`} onClick={() => setAddressId(a.id)}>
                  <div className="address-card-radio">{addressId === a.id && <Icon name="check" size={12} stroke={3}/>}</div>
                  <div className="address-card-body"><div className="address-card-label">{a.label} {a.default && <span className="default-pill">Default</span>}</div><div className="address-card-name">{a.name}</div><div className="address-card-lines">{a.line1}{a.line2 ? `, ${a.line2}` : ''}, {a.city}, {a.state} {a.postal}</div></div>
                </button>
              ))}
            </div>
            <div className="list-header" style={{paddingLeft: 0, marginTop: 16}}>Delivery</div>
            <div className="checkout-rates">
              {SHIPPING_RATES.map(r => {
                const subtotal = summary.subtotal - summary.discount;
                const isFree = r.threshold != null && subtotal >= r.threshold;
                return <button key={r.id} className={`rate-card ${shipRateId === r.id ? 'selected' : ''}`} onClick={() => setShipRateId(r.id)}><div className="rate-card-radio">{shipRateId === r.id && <Icon name="check" size={12} stroke={3}/>}</div><div className="rate-card-body"><div className="rate-card-name">{r.name}</div><div className="rate-card-desc">{r.desc}</div></div><div className="rate-card-price num">{isFree || r.price === 0 ? 'FREE' : money(r.price)}</div></button>;
              })}
            </div>
            <button className="btn" onClick={goNext}>Continue to payment <Icon name="arrow-right" size={16}/></button>
          </>)}
          {((needsShipping && step === 2) || (!needsShipping && step === 1)) && (<>
            {needsShipping && (
              <div className="checkout-confirm-block"><div className="list-header" style={{paddingLeft: 0}}>Ship to</div><div className="confirm-address"><Icon name="pin" size={16}/><div><div>{addresses.find(a => a.id === addressId)?.name}</div><div className="muted">{addresses.find(a => a.id === addressId)?.line1}, {addresses.find(a => a.id === addressId)?.city}</div></div></div></div>
            )}
            <div className="checkout-confirm-block"><div className="list-header" style={{paddingLeft: 0}}>Payment</div><div className="payment-method"><Icon name="card" size={20}/><div><div className="payment-method-name">Visa •• 4242</div><div className="muted">Expires 08/28 · demo</div></div><Icon name="check" size={16}/></div></div>
            <CheckoutTotals summary={summary}/>
            <button className="btn" disabled={processing} onClick={pay}>{processing ? 'Processing…' : summary.cta}</button>
            <div className="plan-legal"><Icon name="lock" size={10}/> Secured by Stripe.</div>
          </>)}
        </div>
      </div>
    </>
  );
};

const CheckoutTotals = ({ summary }) => (
  <div className="checkout-totals">
    <div className="total-row"><span>Subtotal</span><span className="num">{money(summary.subtotal)}</span></div>
    {summary.discount > 0 && <div className="total-row discount"><span>Member discount</span><span className="num">−{money(summary.discount)}</span></div>}
    {summary.hasShipping && <div className="total-row"><span>Shipping</span><span className="num">{summary.shipping === 0 ? 'FREE' : money(summary.shipping)}</span></div>}
    {summary.tax > 0 && <div className="total-row tax"><span>Tax</span><span className="num">{money(summary.tax)}</span></div>}
    <div className="total-row grand"><span>Total</span><span className="num">{money(summary.total)}</span></div>
  </div>
);
window.Checkout = Checkout;
