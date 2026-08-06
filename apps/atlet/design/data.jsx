// data.jsx — workouts seed data + helpers

const SEED_WORKOUTS = [
  {
    id: 'w1',
    name: 'Sunrise 5k',
    type: 'distance',
    target: 5,
    unit: 'km',
    paceTarget: '5:30', // min/km
    structure: 'single',
    notes: 'Steady aerobic, river loop',
    color: '#D2522B',
    last: '2d ago',
    streak: 4,
  },
  {
    id: 'w2',
    name: 'Push Day',
    type: 'reps',
    target: 60,
    unit: 'reps',
    tempo: 3, // 3 seconds per rep
    structure: 'multi',
    steps: [
      { name: 'Push-ups', reps: 20 },
      { name: 'Pike push-ups', reps: 15 },
      { name: 'Dips', reps: 12 },
      { name: 'Diamond push-ups', reps: 13 },
    ],
    notes: 'Bodyweight pyramid',
    color: '#D2522B',
    last: 'Yesterday',
    streak: 7,
    restDur: 45,
  },
  {
    id: 'w3',
    name: 'Tabata Burnout',
    type: 'time',
    target: 240, // 4 minutes
    unit: 'sec',
    structure: 'multi',
    steps: [
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
      { name: 'Rest', dur: 10 },
      { name: 'Work', dur: 20 },
    ],
    notes: '8 rounds, all-out effort',
    color: '#D2522B',
    last: '4d ago',
    streak: 2,
    restDur: 0, // Tabata has its own rest steps built in
  },
  {
    id: 'w4',
    name: 'Mobility Flow',
    type: 'time',
    target: 600, // 10 min
    unit: 'sec',
    structure: 'single',
    notes: 'Hips, shoulders, t-spine',
    color: '#D2522B',
    last: '1w ago',
    streak: 1,
  },
  {
    id: 'w5',
    name: 'Slow Squats',
    type: 'reps',
    target: 30,
    unit: 'reps',
    tempo: 4, // 4 seconds per rep
    structure: 'single',
    notes: 'Controlled tempo, 4s/rep',
    color: '#D2522B',
    last: '3d ago',
    streak: 4,
  },
];

// dashboard data
const WEEK_VOLUME = [22, 0, 38, 18, 0, 45, 28]; // last 7 days, minutes
const WEEK_LABELS = ['M','T','W','T','F','S','S'];
const HEAT_DATA = (() => {
  // 8 weeks × 7 days, value 0-3
  const arr = [];
  for (let w = 0; w < 8; w++) {
    const week = [];
    for (let d = 0; d < 7; d++) {
      const r = Math.random();
      week.push(r < 0.4 ? 0 : r < 0.7 ? 1 : r < 0.9 ? 2 : 3);
    }
    arr.push(week);
  }
  // make recent week look good
  arr[7] = [2,0,3,2,0,3,2];
  arr[6] = [1,2,0,2,3,1,0];
  return arr;
})();
const TREND_POINTS = [82, 95, 110, 88, 130, 142, 155]; // weeks of total minutes

// helpers
const fmtSec = (s) => {
  s = Math.max(0, Math.floor(s));
  const m = Math.floor(s / 60);
  const sec = s % 60;
  return `${String(m).padStart(2,'0')}:${String(sec).padStart(2,'0')}`;
};

const fmtSecMs = (s) => {
  s = Math.max(0, s);
  const m = Math.floor(s / 60);
  const sec = Math.floor(s % 60);
  const cs = Math.floor((s * 100) % 100);
  return `${String(m).padStart(2,'0')}:${String(sec).padStart(2,'0')}.${String(cs).padStart(2,'0')}`;
};

const TYPE_META = {
  time: { label: 'Time', sub: 'Countdown' },
  reps: { label: 'Reps', sub: 'Tap to log' },
  distance: { label: 'Distance', sub: 'Pace tracker' },
};

// ===== v2 commerce entities (APPENDED — v1 content above is untouched) =====
// Three revenue engines: subscriptions, plant-based ecommerce, donations.
// Product imagery: real Unsplash photos (free commercial use, no attribution).
const money = (n) => `$${n.toFixed(2)}`;
const uid = (p) => `${p}-${Date.now().toString(36)}${Math.floor(Math.random()*1000)}`;

const SEED_PRODUCTS = [
  { id: 'p-1', name: 'Pea + Rice Protein', brand: 'Atlet Fuel', category: 'protein',
    price: 42.00, compareAt: 49.00, size: '1 kg', servings: 33, protein: 24, kcal: 130,
    rating: 4.8, reviews: 1240, badges: ['Bestseller'],
    img: 'img/p1-protein.jpg',
    desc: 'Complete amino profile from pea + brown rice isolate. Unflavoured, no gums, no stevia.',
    ingredients: ['Pea protein isolate', 'Brown rice protein isolate', 'Natural cocoa (chocolate only)'],
    stock: 120, subEligible: true },
  { id: 'p-2', name: 'Hemp Protein', brand: 'Atlet Fuel', category: 'protein',
    price: 38.00, size: '500 g', servings: 16, protein: 15, kcal: 150,
    rating: 4.5, reviews: 410, badges: ['Omega-3'],
    img: 'img/p2-hemp.jpg',
    desc: 'Cold-pressed hemp seed protein with natural omega-3 and fibre. Earthy, whole-food.',
    ingredients: ['Cold-pressed hemp seed protein', 'Natural fibre'],
    stock: 64, subEligible: true },
  { id: 'p-3', name: 'Oat Protein Shake', brand: 'Field Kitchen', category: 'rtd',
    price: 4.50, size: '330 ml', servings: 1, protein: 20, kcal: 180,
    rating: 4.4, reviews: 920, badges: ['Ready-to-drink'],
    img: 'img/p3-oatshake.jpg',
    desc: 'Smooth oat-based shake, 20g protein, no added sugar. Grab-and-go recovery.',
    ingredients: ['Oat protein', 'Pea protein', 'Filtered water', 'Sea salt', 'Natural vanilla'],
    stock: 240, subEligible: false },
  { id: 'p-4', name: 'Date + Nut Bar', brand: 'Trail Co', category: 'bars',
    price: 2.80, size: '50 g', servings: 1, protein: 10, kcal: 200,
    rating: 4.7, reviews: 2100, badges: ['Bestseller','6 ingredients'],
    img: 'img/p4-datenutbar.jpg',
    desc: 'Six whole-food ingredients: dates, almonds, pea crisps, cocoa, salt, vanilla.',
    ingredients: ['Medjool dates', 'Almonds', 'Pea protein crisps', 'Raw cocoa', 'Sea salt', 'Vanilla extract'],
    stock: 500, subEligible: true },
  { id: 'p-5', name: 'Tart Cherry Recover', brand: 'Atlet Fuel', category: 'recovery',
    price: 34.00, size: '500 ml', servings: 25, protein: 0, kcal: 30,
    rating: 4.6, reviews: 510, badges: ['Anti-inflammatory'],
    img: 'img/p5-tartcherry.jpg',
    desc: 'Concentrated tart cherry juice for muscle soreness and sleep. 25 doses.',
    ingredients: ['Concentrated Montmorency tart cherry juice'],
    stock: 96, subEligible: true },
  { id: 'p-6', name: 'Electrolyte Mix', brand: 'Atlet Fuel', category: 'recovery',
    price: 22.00, size: '300 g', servings: 60, protein: 0, kcal: 15,
    rating: 4.5, reviews: 1320, badges: ['Bestseller'],
    img: 'img/p6-electrolyte.jpg',
    desc: 'Sodium, potassium, magnesium from sea salt and coconut water. Zero sugar.',
    ingredients: ['Sea salt', 'Coconut water powder', 'Magnesium citrate', 'Potassium chloride'],
    stock: 150, subEligible: true },
];
const CATEGORIES = [
  { id: 'all', label: 'All' }, { id: 'protein', label: 'Protein' },
  { id: 'rtd', label: 'Shakes' }, { id: 'bars', label: 'Bars' }, { id: 'recovery', label: 'Recovery' },
];
const SEED_ADDRESSES = [
  { id: 'addr-1', label: 'Home', default: true, name: 'Alex Kova', line1: '24 Maple Heights', line2: 'Apt 3B',
    city: 'Portland', state: 'OR', postal: '97201', country: 'United States', phone: '+1 503 555 0142' },
];
const SHIPPING_RATES = [
  { id: 'ship-standard', name: 'Standard', desc: '3–5 business days', price: 5.00 },
  { id: 'ship-free', name: 'Free over $50', desc: '3–5 business days', price: 0, threshold: 50 },
];
const SEED_ORDERS = [
  { id: 'ord-1042', number: '1042', date: '2026-06-10', status: 'delivered',
    items: [ { name: 'Pea + Rice Protein', qty: 1, price: 42.00 }, { name: 'Electrolyte Mix', qty: 2, price: 22.00 } ],
    subtotal: 86.00, shipping: 0, tax: 7.74, total: 93.74, paid: true },
];
const SEED_SUBSCRIPTIONS = [
  { id: 'sub-prod-1', tier: 'product', status: 'active', name: 'Pea + Rice Protein', planName: 'Monthly refill',
    price: 37.80, cadence: 'monthly', nextDate: '2026-07-10' },
];
const APP_PLANS = [
  { id: 'free', name: 'Free', tag: 'Starter', priceM: 0, priceY: 0, features: ['3 workouts','Basic tracking','Shop access'] },
  { id: 'pro', name: 'Pro', tag: 'Most popular', featured: true, priceM: 4.99, priceY: 39.99, features: ['Unlimited workouts','10% off Shop','Priority support'] },
  { id: 'supporter', name: 'Supporter', tag: 'Thank you', priceM: 9.99, priceY: 79.99, features: ['Everything in Pro','15% off + free shipping','Supporter badge'] },
];
const SEED_INVOICES = [
  { id: 'inv-1', number: '2026-0042', date: '2026-06-10', kind: 'product', amount: 93.74, status: 'paid',
    method: 'Visa •• 4242', orderId: 'ord-1042',
    subtotal: 86.00, shipping: 0, tax: 7.74, total: 93.74,
    items: [ { name: 'Pea + Rice Protein', qty: 1, price: 42.00 }, { name: 'Electrolyte Mix', qty: 2, price: 22.00 } ] },
  { id: 'inv-2', number: '2026-0041', date: '2026-05-10', kind: 'product', amount: 42.00, status: 'paid',
    method: 'Visa •• 4242', orderId: 'ord-1041',
    subtotal: 42.00, shipping: 0, tax: 0, total: 42.00,
    items: [ { name: 'Pea + Rice Protein', qty: 1, price: 42.00 } ] },
];
const SEED_DONATIONS = [
  { id: 'don-1', date: '2026-05-01', amount: 5.00, message: 'Love the app!', recurring: false, method: 'Visa •• 4242' },
  { id: 'don-2', date: '2026-06-01', amount: 10.00, message: '', recurring: false, method: 'Visa •• 4242' },
];
const SEED_PAYMENT_METHODS = [
  { id: 'pm-1', brand: 'visa', last4: '4242', expMonth: 11, expYear: 2027, default: true, name: 'Alex Kova' },
  { id: 'pm-2', brand: 'mastercard', last4: '8210', expMonth: 3, expYear: 2026, default: false, name: 'Alex Kova' }, // expired-boundary
];
const SEED_PROFILE = { name: 'Alex Kova', email: 'alex@atlet.app', avatar: 'AK', plan: 'free' };
const TAX_RATE = 0.09;

const cartSubtotal = (cart) => cart.reduce((s, l) => { const p = SEED_PRODUCTS.find(x=>x.id===l.productId); return s + (p?p.price*l.qty:0); }, 0);
const resolveShipping = (subtotal, rateId) => {
  const r = SHIPPING_RATES.find(x=>x.id===rateId) || SHIPPING_RATES[0];
  if (r.threshold != null && subtotal >= r.threshold) return { ...r, price: 0 };
  return r;
};

Object.assign(window, {
  SEED_WORKOUTS, WEEK_VOLUME, WEEK_LABELS, HEAT_DATA, TREND_POINTS,
  fmtSec, fmtSecMs, TYPE_META,
  SEED_PRODUCTS, CATEGORIES, SEED_ADDRESSES, SHIPPING_RATES, SEED_ORDERS,
  SEED_SUBSCRIPTIONS, APP_PLANS, SEED_INVOICES, SEED_DONATIONS, SEED_PAYMENT_METHODS, SEED_PROFILE,
  TAX_RATE, money, uid, cartSubtotal, resolveShipping
});
