// icons.jsx — minimal stroke-icon set
const Icon = ({ name, size = 20, stroke = 1.6, ...rest }) => {
  const common = { width: size, height: size, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: stroke, strokeLinecap: 'round', strokeLinejoin: 'round', ...rest };
  switch (name) {
    case 'play':    return <svg {...common}><path d="M7 5l12 7-12 7V5z" fill="currentColor" stroke="none"/></svg>;
    case 'pause':   return <svg {...common}><rect x="6" y="5" width="4" height="14" fill="currentColor" stroke="none"/><rect x="14" y="5" width="4" height="14" fill="currentColor" stroke="none"/></svg>;
    case 'reset':   return <svg {...common}><path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v5h5"/></svg>;
    case 'check':   return <svg {...common}><path d="M5 12l5 5L20 7"/></svg>;
    case 'plus':    return <svg {...common}><path d="M12 5v14M5 12h14"/></svg>;
    case 'close':   return <svg {...common}><path d="M6 6l12 12M18 6L6 18"/></svg>;
    case 'back':    return <svg {...common}><path d="M15 6l-6 6 6 6"/></svg>;
    case 'edit':    return <svg {...common}><path d="M14 4l6 6L9 21H3v-6L14 4z"/></svg>;
    case 'more':    return <svg {...common}><circle cx="6" cy="12" r="1.3" fill="currentColor"/><circle cx="12" cy="12" r="1.3" fill="currentColor"/><circle cx="18" cy="12" r="1.3" fill="currentColor"/></svg>;
    case 'clock':   return <svg {...common}><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>;
    case 'reps':    return <svg {...common}><path d="M4 7h6M4 12h12M4 17h8"/><circle cx="18" cy="7" r="2.5"/></svg>;
    case 'distance':return <svg {...common}><path d="M12 22s7-7.5 7-13a7 7 0 0 0-14 0c0 5.5 7 13 7 13z"/><circle cx="12" cy="9" r="2.5"/></svg>;
    case 'flame':   return <svg {...common}><path d="M12 3s4 4 4 8a4 4 0 0 1-8 0c0-2 1-3 1-3s-2 1-2 4a6 6 0 0 0 12 0c0-5-7-9-7-9z"/></svg>;
    case 'bolt':    return <svg {...common}><path d="M13 2L4 14h7l-1 8 9-12h-7l1-8z" fill="currentColor" stroke="none"/></svg>;
    case 'sound':   return <svg {...common}><path d="M11 5L6 9H3v6h3l5 4V5z"/><path d="M16 9a4 4 0 0 1 0 6"/></svg>;
    case 'settings':return <svg {...common}><circle cx="12" cy="12" r="3"/><path d="M19 12a7 7 0 0 0-.1-1.2l2-1.5-2-3.4-2.3.9a7 7 0 0 0-2.1-1.2L14 3h-4l-.5 2.6a7 7 0 0 0-2.1 1.2l-2.3-.9-2 3.4 2 1.5A7 7 0 0 0 5 12a7 7 0 0 0 .1 1.2l-2 1.5 2 3.4 2.3-.9a7 7 0 0 0 2.1 1.2L10 21h4l.5-2.6a7 7 0 0 0 2.1-1.2l2.3.9 2-3.4-2-1.5c.1-.4.1-.8.1-1.2z"/></svg>;
    case 'trash':   return <svg {...common}><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/></svg>;
    case 'chevron-left': return <svg {...common}><path d="M15 6l-6 6 6 6"/></svg>;
    case 'chevron-right': return <svg {...common}><path d="M9 6l6 6-6 6"/></svg>;
    case 'heart':   return <svg {...common}><path d="M12 20L3 11Q0 8 3 5Q6 2 9 4.5Q12 6.5 12 6.5Q12 6.5 15 4.5Q18 2 21 5Q24 8 21 11Z" fill="currentColor" stroke="none"/></svg>;
    case 'drop':    return <svg {...common}><path d="M12 3s6 7 6 11a6 6 0 0 1-12 0c0-4 6-11 6-11z"/></svg>;
    case 'ruler':   return <svg {...common}><path d="M3 8l5-5 13 13-5 5z"/><path d="M7 8l2 2M10 5l2 2M13 11l2 2M16 8l2 2M10 14l2 2"/></svg>;
    case 'volume':  return <svg {...common}><path d="M11 5L6 9H3v6h3l5 4V5z"/><path d="M16 9a4 4 0 0 1 0 6M19 6a8 8 0 0 1 0 12"/></svg>;
    case 'bell':    return <svg {...common}><path d="M6 17V11a6 6 0 0 1 12 0v6l2 2H4l2-2zM10 21h4"/></svg>;
    case 'pulse':   return <svg {...common}><path d="M3 12h4l2-6 4 12 2-6h6"/></svg>;
    case 'arrow-right': return <svg {...common}><path d="M5 12h14M13 6l6 6-6 6"/></svg>;
    case 'download':return <svg {...common}><path d="M12 4v12M6 12l6 6 6-6M4 21h16"/></svg>;
    case 'upload':  return <svg {...common}><path d="M12 20V8M6 12l6-6 6 6M4 3h16"/></svg>;
    case 'info':    return <svg {...common}><circle cx="12" cy="12" r="9"/><path d="M12 8h.01M11 12h1v5h1"/></svg>;
    case 'doc':     return <svg {...common}><path d="M14 3H6v18h12V7l-4-4z"/><path d="M14 3v4h4M9 13h6M9 17h6"/></svg>;
    case 'lock':    return <svg {...common}><rect x="5" y="11" width="14" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/></svg>;
    case 'help':    return <svg {...common}><circle cx="12" cy="12" r="9"/><path d="M9 9a3 3 0 1 1 4.5 2.6c-.8.5-1.5 1-1.5 2V15M12 18.5h.01"/></svg>;
    case 'star':    return <svg {...common}><path d="M12 3l2.6 6 6.4.6-4.8 4.4 1.4 6.4L12 17l-5.6 3.4L7.8 14 3 9.6 9.4 9z" fill="currentColor" stroke="none"/></svg>;
    // --- v2 commerce icons (appended; v1 set above untouched) ---
    case 'shop':       return <svg {...common}><path d="M4 9h16l-1 11H5L4 9z"/><path d="M9 9V6a3 3 0 0 1 6 0v3"/></svg>;
    case 'cart':       return <svg {...common}><circle cx="9" cy="20" r="1.4"/><circle cx="17" cy="20" r="1.4"/><path d="M3 4h2l2.5 12h11L21 7H6"/></svg>;
    case 'person':     return <svg {...common}><circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/></svg>;
    case 'truck':      return <svg {...common}><path d="M3 6h11v9H3z"/><path d="M14 9h4l3 3v3h-7z"/><circle cx="7" cy="18" r="1.6"/><circle cx="17" cy="18" r="1.6"/></svg>;
    case 'pin':        return <svg {...common}><path d="M12 22s7-7.5 7-13a7 7 0 0 0-14 0c0 5.5 7 13 7 13z"/><circle cx="12" cy="9" r="2.5"/></svg>;
    case 'card':       return <svg {...common}><rect x="3" y="5" width="18" height="14" rx="2"/><path d="M3 10h18M7 15h4"/></svg>;
    case 'receipt':    return <svg {...common}><path d="M5 3h14v18l-2-1.5L15 21l-2-1.5L11 21l-2-1.5L7 21l-2-1.5z"/><path d="M9 8h6M9 12h6M9 16h3"/></svg>;
    case 'leaf':       return <svg {...common}><path d="M5 19c0-7 5-12 14-12 0 9-5 14-12 14-1 0-2-1-2-2z"/><path d="M9 15c2-2 4-3 7-4"/></svg>;
    case 'gift':       return <svg {...common}><path d="M4 11h16v9H4z"/><path d="M4 7h16v4H4z"/><path d="M12 7v13M12 7S10 3 8 4s1 3 4 3zM12 7s2-4 4-3-1 3-4 3z"/></svg>;
    case 'sync':       return <svg {...common}><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/><path d="M3 21v-5h5"/></svg>;
    case 'minus':      return <svg {...common}><path d="M5 12h14"/></svg>;
    case 'search':     return <svg {...common}><circle cx="11" cy="11" r="7"/><path d="M21 21l-4-4"/></svg>;
    case 'tag':        return <svg {...common}><path d="M3 12V3h9l9 9-9 9z"/><circle cx="7.5" cy="7.5" r="1.3"/></svg>;
    case 'shield':     return <svg {...common}><path d="M12 3l8 3v6c0 5-4 8-8 9-4-1-8-4-8-9V6z"/><path d="M9 12l2 2 4-4"/></svg>;
    case 'box':        return <svg {...common}><path d="M3 7l9-4 9 4v10l-9 4-9-4V7z"/><path d="M3 7l9 4 9-4M12 11v10"/></svg>;
    default: return null;
  }
};

window.Icon = Icon;
