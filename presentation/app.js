const slides = [
  {
    eyebrow: "第一阶段",
    title: "网页刚开始很简单",
    lead: "内容很少时，重复整理整页也很难被察觉。",
    visual: `<img src="assets/07-short-page.png" alt="小明编辑只有几段文字的简单网页">`,
    notes: `一开始，这个网页非常简单。页面只有几个标题和几段文字。小明修改一个字，右侧很快就能看到变化。\n\n这个阶段，即使每次都重新整理整篇内容，用户也不容易察觉，因为内容实在太少了。很多页面在早期看起来都没有性能问题，真正的问题往往是在内容规模不断增长之后才出现。`
  },
  {
    eyebrow: "规模增长",
    title: "网页越来越长",
    lead: "章节、表格、图片和图表，把一张简单页面变成了长内容。",
    visual: `<img src="assets/02-page-grows.png" alt="小明为网页添加章节表格图片和图表">`,
    notes: `小明继续往里面加功能。页面有了更多章节，加入了表格、代码块、图片和 Mermaid 图表。\n\n用户还是只做很小的操作，比如修改一段文字、滚动几屏或者打开一个章节。但网页背后需要处理的东西，已经比最开始多了很多。如果还按照最初的方式处理，页面就会开始变慢。`
  },
  {
    eyebrow: "问题一",
    title: "改一个字，却全部重做",
    lead: "局部改动被放大成一次完整的页面整理。",
    visual: `<img src="assets/03-full-rebuild.png" alt="小明修改一处但整张网页被重新整理">`,
    notes: `小明只修改了中间章节的一句话，网页却把从第一章到最后一章的内容全部重新整理了一遍。\n\n这就像一份 100 页的报告只改了一个错别字，却要重新排版、重新打印 100 页。网页越长，每次把全部内容重新处理一遍，浪费就越明显。`
  },
  {
    eyebrow: "第一道减法",
    title: "连续输入，合并成一次处理",
    lead: "少跑几趟，比把每一趟都跑快更直接。",
    visual: `<img src="assets/04-debounce.png" alt="小明连续输入后合并更新">`,
    notes: `用户输入一句话时，可能连续敲下十几个字。如果每个字都立刻触发一次整理，就相当于客人每修改一个字，服务员都跑一趟后厨。\n\n更合理的办法是等用户短暂停下来，再把最新内容统一送过去。它首先减少的是工作的次数。这个方法通常叫防抖。`
  },
  {
    eyebrow: "前台与后台",
    title: "整理网页时，不挡住用户",
    lead: "前台继续接收操作，后台负责整理长内容。",
    visual: `<img src="assets/05-background-version.png" alt="小明编辑网页时后台整理并保护最新结果">`,
    notes: `即使减少了次数，整理长网页仍然可能花一些时间。如果小明必须停下来等整理完成，体验还是会变差。\n\n所以整理工作交给后台，前台继续响应输入和滚动。每份工作还要带一个编号，因为后台完成的顺序不一定和开始顺序一样。只有最新编号的结果可以回到页面，旧结果晚到也不能覆盖新结果。`
  },
  {
    eyebrow: "第二道减法",
    title: "只重新整理变化区域",
    lead: "没有变化的章节，继续使用已经完成的结果。",
    visual: `<img src="assets/06-incremental-update.png" alt="小明只更新网页变化的第三章">`,
    notes: `接下来，小明开始减少每次整理的范围。网页被拆成标题、段落、列表、代码块、表格等内容区域。\n\n用户只修改第三章的一段文字，就只重新整理第三章，其他章节继续使用之前已经整理好的结果。它和防抖解决的是不同问题：防抖减少做多少次，增量更新减少每次做多少。`
  },
  {
    eyebrow: "安全边界",
    title: "有些变化会牵动整篇内容",
    lead: "能确认局部影响就局部处理，看不准就重新检查全文。",
    visual: `<img src="assets/07-incremental-boundary.png" alt="普通段落局部变化与结构变化牵动多处的对比">`,
    notes: `局部更新有一个前提：这次修改确实只影响局部。普通段落里改几个字通常比较安全，但列表层级、表格分隔线和代码块边界可能影响更大的范围。\n\n能确认只影响局部，就局部处理；判断不清楚，就重新整理全部内容。可以少做工作，但不能让页面内容变错。`
  },
  {
    eyebrow: "问题二",
    title: "用户看首屏，网页却准备全文",
    lead: "看不见的内容，同样占据摆放和维护成本。",
    visual: `<img src="assets/08-all-at-once.png" alt="长网页一次性加载大量内容导致内存占用上升">`,
    notes: `就算每个章节整理得很快，如果浏览器一打开就把整篇长网页全部放进去，页面仍然会变重。\n\n用户可能只看到首屏，网页却已经提前准备了后面几十个章节和所有图片。这像展厅刚开门就把所有展品全部搬上展台，观众只看眼前几件，场地却承担了全部成本。`
  },
  {
    eyebrow: "总结",
    title: "少触发，少重做，晚加载",
    lead: "性能优化的核心，是避免当前没有必要的工作。",
    visual: `<img src="assets/14-summary.png" alt="少触发、少重做、晚加载总结">`,
    notes: `把小明做的事情串起来看：连续输入时合并处理；耗时工作交给后台；旧结果不能覆盖新结果；只重新整理变化区域；只准备当前视野附近的内容；图片和图表真正出现时再加载。\n\n页面性能优化的本质，不是让所有事情都神奇地变快，而是尽量不做那些当前没有必要的事情。`
  }
];

const slideOrder = [
  "网页刚开始很简单",
  "网页越来越长",
  "改一个字，却全部重做",
  "只重新整理变化区域",
  "有些变化会牵动整篇内容",
  "连续输入，合并成一次处理",
  "整理网页时，不挡住用户",
  "用户看首屏，网页却准备全文",
  "少触发，少重做，晚加载"
];
slides.sort((a, b) => slideOrder.indexOf(a.title) - slideOrder.indexOf(b.title));

slides.unshift({
  eyebrow: "技术分享",
  title: "长内容网页，如何避免越做越卡？",
  lead: "从一个 Markdown 编辑器，看内容增长之后的性能取舍。",
  visual: `<img src="assets/00-title.png" alt="长内容网页性能优化标题插画">`,
  notes: `今天分享一个很具体的问题：一个内容网页从几段文字开始，逐渐加入章节、表格、图片和图表之后，怎样避免越做越卡。\n\n这次不从抽象概念开始，而是沿着程序员小明做网页的过程，看他遇到问题之后，分别做了哪些取舍。`
});

let index = Number.parseInt(location.hash.slice(1), 10) - 1;
if (!Number.isInteger(index) || index < 0 || index >= slides.length) index = 0;
const slideEl = document.querySelector('#slide');

function render() {
  const item = slides[index];
  slideEl.innerHTML = `
    <div class="visual"><div class="scene-content">${item.visual}</div></div>
    `;
  history.replaceState(null, '', `#${index + 1}`);
}

function move(delta) {
  const nextIndex = Math.max(0, Math.min(slides.length - 1, index + delta));
  if (nextIndex !== index) { index = nextIndex; render(); }
}

slideEl.addEventListener('click', event => {
  if (event.clientX < window.innerWidth / 2) move(-1);
  else move(1);
});

document.addEventListener('keydown', event => {
  if (['ArrowRight', 'PageDown', ' '].includes(event.key)) { event.preventDefault(); move(1); }
  if (['ArrowLeft', 'PageUp'].includes(event.key)) { event.preventDefault(); move(-1); }
  if (event.key.toLowerCase() === 'f') document.documentElement.requestFullscreen?.();
});

render();
