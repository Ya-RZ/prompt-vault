import { App as AntdApp, ConfigProvider } from 'antd';
import zhCN from 'antd/locale/zh_CN';
import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';

import App from './App';
import './styles.css';

const container = document.getElementById('root');
if (!container) {
  throw new Error('未找到 #root 挂载点');
}

createRoot(container).render(
  <StrictMode>
    {/* 中文界面（AntD 中文文案 + 组件默认 locale） */}
    <ConfigProvider
      locale={zhCN}
      theme={{
        token: {
          // 最小定制：只让密度适配数据密集型界面
          borderRadius: 6,
        },
      }}
    >
      {/* AntdApp 提供 message/modal 的上下文（静态方法在 v6 下不再推荐直接调用） */}
      <AntdApp>
        <BrowserRouter>
          <App />
        </BrowserRouter>
      </AntdApp>
    </ConfigProvider>
  </StrictMode>,
);
