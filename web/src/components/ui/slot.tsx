import * as React from "react";

// 极简 Slot：仅支持把 props 透传给唯一子元素（asChild 场景够用）
export const Slot = React.forwardRef<HTMLElement, React.HTMLAttributes<HTMLElement> & { children?: React.ReactNode }>(
  ({ children, ...props }, ref) => {
    if (React.isValidElement(children)) {
      return React.cloneElement(children as React.ReactElement<any>, { ...props, ref });
    }
    return <span {...props} ref={ref as React.Ref<HTMLSpanElement>}>{children}</span>;
  }
);
Slot.displayName = "Slot";
