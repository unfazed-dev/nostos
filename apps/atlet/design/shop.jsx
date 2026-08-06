// shop.jsx — v2 ADDITION (commerce engine #2). Plant-based ecommerce.
// v1 had no shop; this is net-new code. Product tiles use REAL Unsplash photos
// (free commercial use) — never CSS silhouettes (anti-slop rule). A dead URL
// degrades to a token-styled placeholder (leaf + name), never a broken-image icon.

// ProductImage — real <img> with a graceful fallback. If the URL 404s or is empty,
// swap to a styled placeholder (the category leaf glyph + product initial) so the
// tile/sheet ALWAYS shows imagery, never a broken-image icon.
const ProductImage = ({ p, className }) => {
  const [failed, setFailed] = React.useState(false);
  if (!p.img || failed) {
    return (
      <div className={`${className || ''} product-img-fallback`}>
        <Icon name="leaf" size={36} stroke={1.3}/>
        <span className="product-img-fallback-name">{p.name.split(' ')[0]}</span>
      </div>
    );
  }
  return <img className={className} src={p.img} alt={p.name} loading="lazy" onError={() => setFailed(true)}/>;
};

const ProductTile = ({ p, onOpen }) => {
  const off = p.compareAt ? Math.round((1 - p.price / p.compareAt) * 100) : 0;
  return (
    <button className="product-tile rise" onClick={() => onOpen(p)}>
      <div className="product-tile-img">
        <ProductImage p={p}/>
        {p.badges?.[0] && <span className="tile-badge">{p.badges[0]}</span>}
        {off > 0 && <span className="tile-off">−{off}%</span>}
      </div>
      <div className="product-tile-body">
        <div className="product-tile-brand">{p.brand}</div>
        <div className="product-tile-name">{p.name}</div>
        <div className="product-tile-meta">
          <span className="num protein-chip"><Icon name="leaf" size={10}/> {p.protein}g</span>
          <span className="product-tile-size">{p.size}</span>
        </div>
        <div className="product-tile-price">
          <span className="price-now num">{money(p.price)}</span>
          {p.compareAt && <span className="price-was num">{money(p.compareAt)}</span>}
        </div>
      </div>
    </button>
  );
};

const Shop = ({ cart, cartCount, onAdd, onSetQty, onCheckout, onUpgrade, profile }) => {
  const [category, setCategory] = React.useState('all');
  const [query, setQuery] = React.useState('');
  const [detail, setDetail] = React.useState(null);
  const [cartOpen, setCartOpen] = React.useState(false);

  const filtered = SEED_PRODUCTS.filter(p =>
    (category === 'all' || p.category === category) &&
    (!query || (p.name + p.brand + p.desc).toLowerCase().includes(query.toLowerCase()))
  );
  const inCartQty = (id) => cart.find(x => x.productId === id)?.qty || 0;

  return (
    <div className="page">
      <div className="shop-header">
        <div className="shop-header-top">
          <div>
            <div className="eyebrow"><Icon name="leaf" size={11}/> 100% plant-based</div>
            <div className="h-display" style={{fontSize: 28}}>Fuel.</div>
          </div>
          <button className="cart-fab" onClick={() => setCartOpen(true)} aria-label="Cart">
            <Icon name="cart" size={22}/>
            {cartCount > 0 && <span className="cart-fab-badge">{cartCount}</span>}
          </button>
        </div>
        <div className="search-wrap">
          <Icon name="search" size={16}/>
          <input className="search-input" placeholder="Search protein, bars, recovery…" value={query} onChange={(e) => setQuery(e.target.value)}/>
        </div>
        <div className="filter-row">
          {CATEGORIES.map(c => (
            <button key={c.id} className={`filter-chip ${category === c.id ? 'active' : ''}`} onClick={() => setCategory(c.id)}>
              {c.label}
              {c.id !== 'all' && <span className="filter-count">{SEED_PRODUCTS.filter(p => p.category === c.id).length}</span>}
            </button>
          ))}
        </div>
      </div>
      <div className="scrollable">
        {profile.plan === 'free' && (
          <button className="shop-upsell rise" onClick={onUpgrade}>
            <div className="shop-upsell-body">
              <div className="eyebrow" style={{color:'var(--accent)'}}><Icon name="tag" size={11}/> Save 10–15%</div>
              <div className="shop-upsell-title">Members save on every order</div>
            </div>
            <Icon name="arrow-right" size={18}/>
          </button>
        )}
        <div className="product-grid">
          {filtered.map(p => <ProductTile key={p.id} p={p} onOpen={setDetail}/>)}
        </div>
        {filtered.length === 0 && (
          <div className="empty-state"><div className="empty-title">No products found</div><div className="empty-sub">Try a different search.</div></div>
        )}
        <div className="shop-trust">
          <div className="shop-trust-row"><Icon name="truck" size={16}/> Free shipping over $50</div>
          <div className="shop-trust-row"><Icon name="sync" size={16}/> Subscribe &amp; save 10%</div>
          <div className="shop-trust-row"><Icon name="leaf" size={16}/> Always plant-based</div>
          <div className="shop-trust-row"><Icon name="shield" size={16}/> 30-day returns</div>
        </div>
        <div style={{height: 24}}/>
      </div>
      <ProductSheet product={detail} onClose={() => setDetail(null)} onAdd={onAdd} inCartQty={inCartQty(detail?.id)}/>
      <CartSheet open={cartOpen} onClose={() => setCartOpen(false)} cart={cart} onSetQty={onSetQty} onCheckout={() => { setCartOpen(false); onCheckout(); }} profile={profile}/>
    </div>
  );
};

const ProductSheet = ({ product, onClose, onAdd, inCartQty }) => {
  const [qty, setQty] = React.useState(1);
  const [sub, setSub] = React.useState(false);
  React.useEffect(() => { if (product) { setQty(1); setSub(false); } }, [product]);
  const open = !!product;
  if (!open) return null;
  const p = product;
  const off = p.compareAt ? Math.round((1 - p.price / p.compareAt) * 100) : 0;
  const unitPrice = sub ? p.price * 0.9 : p.price;
  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet product-sheet ${open ? 'open' : ''}`}>
        <div className="product-sheet-hero">
          <ProductImage p={p}/>
          <button className="product-close" onClick={onClose} aria-label="Close"><Icon name="close" size={22} stroke={2.2}/></button>
        </div>
        <div className="product-sheet-body">
          <div className="product-brand-row">
            <span className="product-brand">{p.brand}</span>
            <div className="rating"><Icon name="star" size={12}/> {p.rating} <span className="muted">({p.reviews})</span></div>
          </div>
          <div className="product-title">{p.name}</div>
          <div className="product-price-row">
            <span className="price-big num">{money(unitPrice)}</span>
            {p.compareAt && <span className="price-was num">{money(p.compareAt)}</span>}
            {off > 0 && <span className="save-badge">−{off}%</span>}
            <span className="product-size">{p.size} · {p.servings} servings</span>
          </div>
          <div className="product-nutrition">
            <div className="nutri-cell"><div className="nutri-val num">{p.protein}g</div><div className="nutri-lbl">Protein</div></div>
            <div className="nutri-cell"><div className="nutri-val num">{p.kcal}</div><div className="nutri-lbl">kcal</div></div>
            <div className="nutri-cell"><div className="nutri-val num">{p.servings}</div><div className="nutri-lbl">Servings</div></div>
          </div>
          <div className="product-section-label">Description</div>
          <p className="product-desc">{p.desc}</p>
          {p.ingredients && (
            <>
              <div className="product-section-label">Ingredients</div>
              <ul className="product-ingredients">
                {p.ingredients.map((ing, i) => <li key={i}><Icon name="leaf" size={11}/> {ing}</li>)}
              </ul>
            </>
          )}
          {p.badges && <div className="badge-row">{p.badges.map(b => <span key={b} className="product-badge"><Icon name="check" size={10}/> {b}</span>)}</div>}
          {p.subEligible && (
            <div className="sub-toggle-row" onClick={() => setSub(s => !s)}>
              <div className="sub-toggle-info">
                <div className="sub-toggle-title">Subscribe &amp; save <span className="save-badge">−10%</span></div>
                <div className="sub-toggle-sub">{sub ? 'Monthly auto-refill' : 'One-time purchase'}</div>
              </div>
              <button className={`ios-switch ${sub ? 'on' : ''}`} aria-label="Toggle subscription"/>
            </div>
          )}
          <div className="qty-row">
            <div className="qty-label">Quantity</div>
            <div className="qty-stepper">
              <button className="qty-btn" onClick={() => setQty(q => Math.max(1, q-1))}><Icon name="minus" size={14}/></button>
              <span className="qty-val num">{qty}</span>
              <button className="qty-btn" onClick={() => setQty(q => q+1)}><Icon name="plus" size={14}/></button>
            </div>
          </div>
        </div>
        <div className="sheet-foot">
          <button className="btn" onClick={() => { onAdd(p, qty); onClose(); }}>
            {sub ? `Subscribe · ${money(unitPrice * qty)}` : `Add ${qty} · ${money(unitPrice * qty)}`}
          </button>
          {inCartQty > 0 && <div className="already-in-cart">{inCartQty} already in cart</div>}
        </div>
      </div>
    </>
  );
};

const CartSheet = ({ open, onClose, cart, onSetQty, onCheckout, profile }) => {
  const lines = cart.map(l => ({...l, product: SEED_PRODUCTS.find(p => p.id === l.productId)})).filter(l => l.product);
  const subtotal = lines.reduce((s, l) => s + l.product.price * l.qty, 0);
  const discount = profile.plan === 'supporter' ? subtotal * 0.15 : profile.plan === 'pro' ? subtotal * 0.10 : 0;
  const freeShip = subtotal >= 50;
  return (
    <>
      <div className={`scrim ${open ? 'open' : ''}`} onClick={onClose}/>
      <div className={`sheet cart-sheet ${open ? 'open' : ''}`}>
        <div className="grabber"/>
        <div className="sheet-head">
          <button className="nav-plain" onClick={onClose}><Icon name="chevron-left" size={18}/><span>Shop</span></button>
          <div className="sheet-title">Your cart</div>
          <div className="num" style={{color:'var(--ink-3)'}}>{cart.reduce((n,x)=>n+x.qty,0)}</div>
        </div>
        <div className="sheet-body">
          {lines.length === 0 ? (
            <div className="empty-state"><div className="empty-title">Your cart is empty</div><div className="empty-sub">Add plant-based fuel.</div></div>
          ) : (<>
            {lines.map(l => (
              <div key={l.productId} className="cart-line">
                <img className="cart-line-img" src={l.product.img} alt={l.product.name}/>
                <div className="cart-line-body">
                  <div className="cart-line-name">{l.product.name}</div>
                  <div className="cart-line-meta">{l.product.brand} · {l.product.size}</div>
                  <div className="cart-line-price num">{money(l.product.price)}</div>
                </div>
                <div className="qty-stepper small">
                  <button className="qty-btn" onClick={() => onSetQty(l.productId, l.qty - 1)}><Icon name="minus" size={12}/></button>
                  <span className="qty-val num">{l.qty}</span>
                  <button className="qty-btn" onClick={() => onSetQty(l.productId, l.qty + 1)}><Icon name="plus" size={12}/></button>
                </div>
              </div>
            ))}
            <div className="cart-totals">
              <div className="total-row"><span>Subtotal</span><span className="num">{money(subtotal)}</span></div>
              {discount > 0 && <div className="total-row discount"><span>Member discount</span><span className="num">−{money(discount)}</span></div>}
              <div className="total-row"><span>Shipping</span><span className="num">{freeShip ? 'FREE' : 'Calculated at checkout'}</span></div>
            </div>
            {!freeShip && subtotal > 0 && (
              <div className="ship-progress">
                <div className="ship-progress-text">Add <strong>{money(50 - subtotal)}</strong> for free shipping</div>
                <div className="ship-progress-bar"><div style={{width: `${Math.min(100, subtotal/50*100)}%`}}/></div>
              </div>
            )}
          </>)}
        </div>
        {lines.length > 0 && (
          <div className="sheet-foot">
            <button className="btn" onClick={onCheckout}>Checkout · {money(subtotal - discount + (subtotal-discount)*TAX_RATE)}</button>
          </div>
        )}
      </div>
    </>
  );
};

window.Shop = Shop;
window.ProductSheet = ProductSheet;
window.CartSheet = CartSheet;
