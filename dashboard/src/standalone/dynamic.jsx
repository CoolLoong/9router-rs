import { lazy, Suspense } from "react";

export default function dynamic(factory, options = {}) {
  const Component = lazy(async () => {
    const loaded = await factory();
    return loaded?.default ? loaded : { default: loaded };
  });
  return function DynamicComponent(props) {
    const Loading = options.loading;
    return <Suspense fallback={Loading ? <Loading /> : null}><Component {...props} /></Suspense>;
  };
}
